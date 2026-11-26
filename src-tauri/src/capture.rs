//! 拍照与录像：拍照直接把 JPEG 落盘（镜像时才解码翻转），录像时才把 JPEG 解码成
//! RGB rawvideo 交给 libx264。缩略图统一用 JPEG。

use crate::ffmpeg;
use crate::source::{Frame, Thumb};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageFormat, RgbImage};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 默认素材目录：~/Movies/MONITOR。
pub fn captures_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Movies").join("MONITOR")
}

pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建素材目录 {}：{e}", dir.display()))
}

/// 文件名时间戳：20261126-182205。
pub fn stamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    chrono::DateTime::from_timestamp(now.as_secs() as i64, 0)
        .map(|dt| dt.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| format!("{}", now.as_secs()))
}

pub fn unique_name(dir: &Path, prefix: &str, ext: &str) -> (String, PathBuf) {
    let base = format!("{prefix}-{}", stamp());
    let mut name = format!("{base}.{ext}");
    let mut i = 2;
    while dir.join(&name).exists() {
        name = format!("{base}-{i}.{ext}");
        i += 1;
    }
    let path = dir.join(&name);
    (name, path)
}

/// 解码为 RGB（仅镜像/录像/缩略图需要）。
pub fn decode(jpeg: &[u8]) -> Result<RgbImage, String> {
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
        .map_err(|e| format!("画面解码失败：{e}"))?;
    Ok(img.to_rgb8())
}

pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode(img, img.width(), img.height(), image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("JPEG 编码失败：{e}"))?;
    Ok(out)
}

pub fn mirror_jpeg(jpeg: &[u8]) -> Result<Vec<u8>, String> {
    let mut img = decode(jpeg)?;
    image::imageops::flip_horizontal_in_place(&mut img);
    encode_jpeg(&img, 92)
}

/// 按目标高度等比缩放得到缩略图（JPEG）。
pub fn make_thumb(jpeg: &[u8], target_h: u32) -> Option<Thumb> {
    // 缩略图只用于胶片条与列表，质量优先于速度
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).ok()?;
    let img = img.to_rgb8();
    let (mut w, mut h) = (img.width(), img.height());
    if h == 0 || w == 0 {
        return None;
    }
    if h > target_h {
        w = (w as f32 * target_h as f32 / h as f32).round().max(1.0) as u32;
        h = target_h;
    }
    let small = if (w, h) == (img.width(), img.height()) {
        img
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
    };
    let jpeg = encode_jpeg(&small, 80).ok()?;
    Some(Thumb { width: small.width(), height: small.height(), jpeg })
}

/// 保存一帧为 JPEG 照片（可选水平镜像），返回文件名、路径与缩略图。
pub fn save_photo(frame: &Frame, mirror: bool, dir: &Path) -> Result<(String, PathBuf, Thumb), String> {
    ensure_dir(dir)?;
    let (name, path) = unique_name(dir, "photo", "jpg");
    let jpeg = if mirror { mirror_jpeg(&frame.jpeg)? } else { frame.jpeg.as_ref().clone() };
    std::fs::write(&path, &jpeg).map_err(|e| format!("写入照片失败：{e}"))?;
    let thumb = make_thumb(&jpeg, 200).ok_or("生成缩略图失败")?;
    Ok((name, path, thumb))
}

/// 用 ffmpeg 抓视频首帧作为缩略图。
pub fn video_thumb(path: &Path) -> Option<Thumb> {
    let ff = ffmpeg::ffmpeg_path()?;
    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error",
        "-ss", "0", "-i", path.to_str()?, "-frames:v", "1",
        "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "4", "pipe:1",
    ]);
    let out = ffmpeg::run_with_timeout(cmd, Duration::from_secs(10))?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    make_thumb(&out.stdout, 200)
}

fn run_ffmpeg_recording(
    mut child: std::process::Child,
    mut stdin: std::process::ChildStdin,
    rx: Receiver<Frame>,
    path: PathBuf,
    width: u32,
    height: u32,
    start: Instant,
    stderr_sink: Option<Vec<String>>,
) -> Result<(String, PathBuf, f64, Option<Thumb>), String> {
    let _ = stderr_sink;
    let writer = std::thread::spawn(move || -> Result<u64, String> {
        let mut frames = 0u64;
        while let Ok(frame) = rx.recv() {
            let img = match decode(&frame.jpeg) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let img = if img.width() != width || img.height() != height {
                image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle)
            } else {
                img
            };
            if stdin.write_all(img.as_raw()).is_err() {
                break;
            }
            frames += 1;
        }
        drop(stdin);
        Ok(frames)
    });
    let status = child.wait().map_err(|e| format!("录像进程失败：{e}"))?;
    let frames = writer.join().map_err(|_| "录像写入线程崩溃".to_string())?.unwrap_or(0);
    let secs = start.elapsed().as_secs_f64();
    if !status.success() && frames == 0 {
        return Err("录像编码失败（详见状态栏）".to_string());
    }
    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let thumb = video_thumb(&path);
    Ok((name, path, secs, thumb))
}

/// 录像中：接收实时帧，后台线程解码写 rawvideo。
pub struct Recorder {
    tx: Sender<Frame>,
    join: Option<JoinHandle<Result<(String, PathBuf, f64, Option<Thumb>), String>>>,
    start: Instant,
    pub name: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

impl Recorder {
    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn push(&self, frame: Frame) {
        let _ = self.tx.send(frame);
    }

    /// 结束录像并等待封装完成。
    pub fn finish(mut self) -> Result<(String, PathBuf, f64, Option<Thumb>), String> {
        drop(std::mem::replace(&mut self.tx, channel().0));
        match self.join.take() {
            Some(j) => j.join().map_err(|_| "录像线程崩溃".to_string())?,
            None => Err("录像未启动".to_string()),
        }
    }
}

pub fn start_recording(frame: &Frame, fps: f32, dir: &Path) -> Result<Recorder, String> {
    ensure_dir(dir)?;
    let ff = ffmpeg::ffmpeg_path().ok_or("未找到 ffmpeg，无法录像")?;
    let (name, path) = unique_name(dir, "clip", "mp4");
    let (width, height) = (frame.width & !1, frame.height & !1);
    let fps = if fps.is_finite() && fps > 1.0 { fps.round() } else { 30.0 };

    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24",
        "-s", &format!("{width}x{height}"), "-framerate", &format!("{fps}"),
        "-i", "pipe:0",
        "-an", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20",
        "-pix_fmt", "yuv420p", "-movflags", "+faststart",
        path.to_str().ok_or("素材路径无效")?,
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("启动录像失败：{e}"))?;
    let stdin = child.stdin.take().ok_or("无法获取录像输入")?;

    let (tx, rx) = channel::<Frame>();
    let start = Instant::now();
    let thread_path = path.clone();
    let join = std::thread::spawn(move || -> Result<(String, PathBuf, f64, Option<Thumb>), String> {
        run_ffmpeg_recording(child, stdin, rx, thread_path, width, height, start, None)
    });

    Ok(Recorder { tx, join: Some(join), start, name, path, width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        let img = RgbImage::from_pixel(16, 8, image::Rgb([10, 200, 30]));
        encode_jpeg(&img, 90).unwrap()
    }

    #[test]
    fn encodes_and_decodes_jpeg() {
        let jpeg = tiny_jpeg();
        let img = decode(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (16, 8));
    }

    #[test]
    fn thumb_preserves_aspect_ratio() {
        let t = make_thumb(&tiny_jpeg(), 4).expect("thumb");
        assert_eq!(t.height, 4);
        assert_eq!(t.width, 8);
    }

    #[test]
    fn names_are_unique_and_timestamped() {
        let dir = std::env::temp_dir().join(format!("monitor-test-{}", std::process::id()));
        ensure_dir(&dir).unwrap();
        let (n1, p1) = unique_name(&dir, "photo", "jpg");
        std::fs::write(&p1, b"x").unwrap();
        let (n2, _p2) = unique_name(&dir, "photo", "jpg");
        assert_ne!(n1, n2);
        assert!(n2.ends_with("-2.jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn frame_16x8() -> Frame {
        Frame { width: 16, height: 8, jpeg: std::sync::Arc::new(tiny_jpeg()) }
    }

    #[test]
    fn saves_photo_plain_and_mirrored() {
        let dir = std::env::temp_dir().join(format!("monitor-save-{}", std::process::id()));
        ensure_dir(&dir).unwrap();
        let frame = frame_16x8();
        let (name, path, thumb) = save_photo(&frame, false, &dir).unwrap();
        assert!(path.ends_with(&name));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert!(thumb.width > 0 && thumb.height > 0);

        let (mirrored_name, mirrored_path, _) = save_photo(&frame, true, &dir).unwrap();
        assert_ne!(mirrored_name, name);
        let mirrored = std::fs::read(&mirrored_path).unwrap();
        assert_eq!(&mirrored[..2], &[0xFF, 0xD8]);
        assert_ne!(mirrored, bytes, "镜像后应重新编码，字节不同");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_synthetic_frames_with_ffmpeg() {
        if ffmpeg::ffmpeg_path().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("monitor-record-{}", std::process::id()));
        ensure_dir(&dir).unwrap();
        let frame = frame_16x8();
        let rec = start_recording(&frame, 15.0, &dir).unwrap();
        for _ in 0..12 {
            rec.push(frame.clone());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (name, path, secs, _thumb) = rec.finish().unwrap();
        assert!(name.ends_with(".mp4"));
        assert!(secs >= 0.0);
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > 400, "mp4 太小：{len} 字节");
        let head = std::fs::read(&path).unwrap();
        assert!(head.windows(4).any(|w| w == b"ftyp"), "不是有效的 mp4");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! 拍照、录像（ffmpeg rawvideo → H.264 MP4）与缩略图。

use crate::ffmpeg;
use crate::source::{Event, Frame, Thumb};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{sync_channel, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Instant;

pub fn captures_dir() -> PathBuf {
    PathBuf::from(std::env::var("MONITOR_CAPTURE_DIR").unwrap_or_else(|_| "captures".into()))
}

pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string()
}

/// 水平镜像 RGB 画面。
pub fn mirror_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let w = width as usize;
    let stride = w * 3;
    let mut out = vec![0u8; rgb.len()];
    for y in 0..height as usize {
        let row = &rgb[y * stride..(y + 1) * stride];
        let dst = &mut out[y * stride..(y + 1) * stride];
        for x in 0..w {
            dst[x * 3..x * 3 + 3].copy_from_slice(&row[(w - 1 - x) * 3..(w - 1 - x) * 3 + 3]);
        }
    }
    out
}

/// 生成高度不超过 target_h 的等比缩略图。
pub fn make_thumb(width: u32, height: u32, rgb: &[u8], target_h: u32) -> Option<Thumb> {
    let src = image::RgbImage::from_raw(width, height, rgb.to_vec())?;
    let h = target_h.min(height).max(1);
    let w = ((width as f64 * h as f64 / height as f64).round() as u32).max(1);
    let small = image::imageops::resize(&src, w, h, image::imageops::FilterType::Triangle);
    Some(Thumb { width: w, height: h, rgb: small.into_raw() })
}

/// 后台保存当前帧为 JPEG（可镜像），完成后发 PhotoSaved 事件。
pub fn save_photo(frame: &Frame, mirror: bool, dir: &Path, tx: Sender<Event>) {
    let rgb = if mirror {
        mirror_rgb(frame.width, frame.height, &frame.rgb)
    } else {
        frame.rgb.as_ref().clone()
    };
    let (w, h) = (frame.width, frame.height);
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        let result = (|| -> Result<(PathBuf, Thumb), String> {
            std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
            let path = dir.join(format!("IMG_{}.jpg", stamp()));
            let img = image::RgbImage::from_raw(w, h, rgb).ok_or("帧数据尺寸不符")?;
            img.save(&path).map_err(|e| format!("保存失败：{e}"))?;
            let thumb = make_thumb(w, h, img.as_raw(), 96).ok_or("缩略图失败")?;
            Ok((path, thumb))
        })();
        match result {
            Ok((path, thumb)) => {
                let _ = tx.send(Event::PhotoSaved { path, thumb });
            }
            Err(e) => {
                let _ = tx.send(Event::Notice(format!("拍照失败：{e}")));
            }
        }
    });
}

struct RecFrame {
    width: u32,
    height: u32,
    rgb: Arc<Vec<u8>>,
}

pub struct Recorder {
    tx: Option<SyncSender<RecFrame>>,
    pub path: PathBuf,
    pub start: Instant,
}

/// 开始录像：rawvideo 管道 → libx264 MP4。分辨率变化时在 Rust 侧缩放到录制尺寸。
pub fn start_recording(frame: &Frame, fps: f32, dir: &Path, events: Sender<Event>) -> Result<Recorder, String> {
    let ff = ffmpeg::ffmpeg_path().ok_or("未找到 ffmpeg，无法录像")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let path = dir.join(format!("VID_{}.mp4", stamp()));
    let fps = if fps.is_finite() && fps >= 1.0 { fps.clamp(5.0, 60.0).round() as u32 } else { 30 };
    let (w, h) = (frame.width.max(2) & !1, frame.height.max(2) & !1);
    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24",
        "-s", &format!("{w}x{h}"), "-framerate", &fps.to_string(),
        "-i", "pipe:0",
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "20",
        "-pix_fmt", "yuv420p", "-movflags", "+faststart",
    ]);
    cmd.arg(&path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动录像编码器失败：{e}"))?;
    let mut stdin = child.stdin.take().ok_or("无法获取编码器输入")?;
    let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
    if let Some(err) = child.stderr.take() {
        ffmpeg::drain_stderr(err, sink.clone());
    }
    let thumb = make_thumb(frame.width, frame.height, &frame.rgb, 96);
    let (tx, rx) = sync_channel::<RecFrame>(30);
    let path2 = path.clone();
    let start = Instant::now();
    std::thread::spawn(move || {
        while let Ok(f) = rx.recv() {
            let bytes: Vec<u8> = if f.width == w && f.height == h {
                f.rgb.as_ref().clone()
            } else {
                // 源分辨率变化：缩放到录制尺寸
                match image::RgbImage::from_raw(f.width, f.height, f.rgb.as_ref().clone()) {
                    Some(img) => image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle).into_raw(),
                    None => continue,
                }
            };
            if stdin.write_all(&bytes).is_err() {
                break;
            }
        }
        drop(stdin); // 关闭管道，ffmpeg 写入 moov 后退出
        let status = child.wait();
        let secs = start.elapsed().as_secs_f64();
        let error = match status {
            Ok(s) if s.success() => None,
            Ok(s) => {
                let detail = sink.lock().map(|v| v.join(" | ")).unwrap_or_default();
                Some(format!("编码器退出（{s}）{detail}"))
            }
            Err(e) => Some(format!("等待编码器失败：{e}")),
        };
        let _ = events.send(Event::RecordingDone { path: path2, thumb, secs, error });
    });
    Ok(Recorder { tx: Some(tx), path, start })
}

impl Recorder {
    /// 送一帧进编码器；队列满时丢帧（保实时）。
    pub fn push(&self, frame: &Frame) {
        if let Some(tx) = &self.tx {
            match tx.try_send(RecFrame { width: frame.width, height: frame.height, rgb: frame.rgb.clone() }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    /// 停止录像：关闭帧通道，编码线程在后台完成文件收尾。
    pub fn stop(&mut self) {
        self.tx.take();
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_flips_rows() {
        let rgb = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]; // 2x2
        let m = mirror_rgb(2, 2, &rgb);
        assert_eq!(m, vec![4, 5, 6, 1, 2, 3, 10, 11, 12, 7, 8, 9]);
        assert_eq!(mirror_rgb(2, 2, &m), rgb);
    }

    #[test]
    fn thumb_keeps_aspect() {
        let rgb = vec![128u8; 1920 * 1080 * 3];
        let t = make_thumb(1920, 1080, &rgb, 96).unwrap();
        assert_eq!(t.height, 96);
        assert_eq!(t.width, 171); // 1920/1080*96 ≈ 170.67 → 171
        assert_eq!(t.rgb.len(), 171 * 96 * 3);
    }

    #[test]
    fn thumb_rejects_bad_size() {
        assert!(make_thumb(10, 10, &[0; 5], 96).is_none());
    }

    #[test]
    fn stamp_has_millis() {
        let s = stamp();
        assert_eq!(s.len(), 19); // YYYYmmdd_HHMMSS_mmm
        assert!(s.as_bytes()[8] == b'_');
    }
}

//! ffmpeg 定位、带超时的子进程执行、MJPEG 管道帧提取。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn ffmpeg_exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// 各平台常见的安装位置。
#[cfg(target_os = "windows")]
fn common_ffmpeg_paths() -> Vec<PathBuf> {
    ["C:/ffmpeg/bin/ffmpeg.exe", "C:/Program Files/ffmpeg/bin/ffmpeg.exe"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn common_ffmpeg_paths() -> Vec<PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// 缺 ffmpeg 时给用户的安装提示（按平台）。
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "（Windows：winget install Gyan.FFmpeg，或把 ffmpeg.exe 放到应用目录，或设置 MONITOR_FFMPEG 指向它）"
    } else {
        "（macOS：brew install ffmpeg）"
    }
}

pub fn ffmpeg_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MONITOR_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // 与主程序同目录（安装包/绿色版自带 ffmpeg 时优先用自带的，不依赖系统 PATH）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [
                dir.join(ffmpeg_exe_name()),
                dir.join("bin").join(ffmpeg_exe_name()),
            ] {
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    for pb in common_ffmpeg_paths() {
        if pb.is_file() {
            return Some(pb);
        }
    }
    let lister = if cfg!(windows) { "where" } else { "which" };
    let out = crate::proc::command(lister).arg("ffmpeg").output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    None
}

/// 小输出命令的带超时执行（输出必须很小，否则管道会写满）。
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// 把子进程 stderr/stdout 持续读进共享缓冲（保留最后若干行，用于诊断）。
pub fn drain_stderr<R: std::io::Read + Send + 'static>(stderr: R, sink: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if let Ok(mut s) = sink.lock() {
                s.push(line);
                if s.len() > 8 {
                    s.remove(0);
                }
            }
        }
    });
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];
const JPEG_EOI: [u8; 2] = [0xFF, 0xD9];

/// 从字节流里切出完整的 JPEG（ffmpeg -f image2pipe -c:v mjpeg 的输出）。
/// JPEG 熵编码段内 0xFF 后必为 0x00 或合法标记，FFD9 不会出现在数据中间。
pub fn extract_jpegs(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let start = match find_subslice(buf, &JPEG_SOI) {
            Some(i) => i,
            None => {
                // 保留可能是半个 SOI 的尾部
                if buf.len() > 1 {
                    buf.drain(..buf.len() - 1);
                }
                break;
            }
        };
        if start > 0 {
            buf.drain(..start);
        }
        match find_subslice(&buf[2..], &JPEG_EOI) {
            Some(j) => {
                let end = 2 + j + 2;
                out.push(buf[..end].to_vec());
                buf.drain(..end);
            }
            None => break, // 不完整，等后续字节
        }
    }
    out
}

/// 从 JPEG 数据本身解析尺寸（扫描 SOFn 段），无需完整解码。
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        i += 2;
        match marker {
            0x00 | 0xFF | 0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xDA => return None, // 扫描数据先于 SOF，异常
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if i + 7 > jpeg.len() {
                    return None;
                }
                let h = u16::from_be_bytes([jpeg[i + 3], jpeg[i + 4]]) as u32;
                let w = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
                if w == 0 || h == 0 {
                    return None;
                }
                return Some((w, h));
            }
            _ => {
                if i + 1 >= jpeg.len() {
                    return None;
                }
                let len = u16::from_be_bytes([jpeg[i], jpeg[i + 1]]) as usize;
                if len < 2 {
                    return None;
                }
                i += len;
            }
        }
    }
    None
}

#[cfg(test)]
mod jpeg_size_tests {
    use super::*;

    fn jpeg_with_sof(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]); // APP0
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn reads_dimensions_from_sof() {
        assert_eq!(jpeg_size(&jpeg_with_sof(1280, 720)), Some((1280, 720)));
        assert_eq!(jpeg_size(&jpeg_with_sof(1080, 2400)), Some((1080, 2400)));
    }

    #[test]
    fn rejects_non_jpeg_and_truncated() {
        assert_eq!(jpeg_size(b"not a jpeg"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8]), None);
        assert_eq!(jpeg_size(&jpeg_with_sof(0, 720)), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(payload: &[u8]) -> Vec<u8> {
        let mut v = JPEG_SOI.to_vec();
        v.extend_from_slice(payload);
        v.extend_from_slice(&JPEG_EOI);
        v
    }

    #[test]
    fn extracts_complete_jpegs_and_keeps_partial() {
        let a = fake_jpeg(b"hello");
        let b = fake_jpeg(b"world!");
        let mut buf = a.clone();
        buf.extend_from_slice(&b[..4]);
        let got = extract_jpegs(&mut buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], a);
        buf.extend_from_slice(&b[4..]);
        let got = extract_jpegs(&mut buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], b);
        assert!(buf.is_empty());
    }

    #[test]
    fn skips_garbage_between_jpegs() {
        let a = fake_jpeg(b"x");
        let mut buf = b"junk".to_vec();
        buf.extend_from_slice(&a);
        let got = extract_jpegs(&mut buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], a);
    }

    #[test]
    fn jpeg_keeps_lone_ff_tail() {
        let mut buf = vec![0xFF];
        assert!(extract_jpegs(&mut buf).is_empty());
        assert_eq!(buf, vec![0xFF]);
        buf.push(0xD8);
        buf.extend_from_slice(b"ab");
        buf.extend_from_slice(&JPEG_EOI);
        let got = extract_jpegs(&mut buf);
        assert_eq!(got.len(), 1);
    }

}

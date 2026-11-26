//! AVFoundation 设备枚举：本机摄像头 + 通过 USB 连接并信任的 iPhone/iPad
//! （iPhone 会作为视频采集设备出现，即「手机屏幕」信号源）。

use crate::ffmpeg;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct AvDevice {
    pub index: u32,
    pub name: String,
}

pub fn parse_avfoundation(text: &str) -> Vec<AvDevice> {
    let mut in_video = false;
    let mut devs = Vec::new();
    for line in text.lines() {
        if line.contains("AVFoundation video devices") {
            in_video = true;
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            in_video = false;
            continue;
        }
        if !in_video {
            continue;
        }
        // 形如: [AVFoundation indev @ 0x...] [0] FaceTime HD Camera
        if let Some(i) = line.find("] [") {
            let rest = &line[i + 3..];
            if let Some(j) = rest.find("] ") {
                if let Ok(index) = rest[..j].trim().parse::<u32>() {
                    let name = rest[j + 2..].trim().to_string();
                    if !name.is_empty() {
                        devs.push(AvDevice { index, name });
                    }
                }
            }
        }
    }
    devs
}

pub fn list_avfoundation() -> Vec<AvDevice> {
    let Some(ffmpeg) = ffmpeg::ffmpeg_path() else { return Vec::new() };
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""]);
    match ffmpeg::run_with_timeout(cmd, Duration::from_secs(6)) {
        Some(out) => parse_avfoundation(&String::from_utf8_lossy(&out.stderr)),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_video_section_only() {
        let text = "[AVFoundation indev @ 0x7f8] AVFoundation video devices:
                    [AVFoundation indev @ 0x7f8] [0] FaceTime HD Camera
                    [AVFoundation indev @ 0x7f8] [1] iPhone
                    [AVFoundation indev @ 0x7f8] AVFoundation audio devices:
                    [AVFoundation indev @ 0x7f8] [0] MacBook Pro Microphone
";
        let devs = parse_avfoundation(text);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].name, "FaceTime HD Camera");
        assert_eq!(devs[1].index, 1);
        assert_eq!(devs[1].name, "iPhone");
    }
}

//! 相机枚举：macOS 用 AVFoundation，Windows 用 DirectShow。
//! macOS 上通过 USB 连接并信任的 iPhone/iPad 也会作为采集设备出现（即「手机屏幕」信号源）。

use crate::ffmpeg;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
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

/// 解析 ffmpeg -f dshow -list_devices 的输出，例如：
/// [dshow @ 000...] "HD Webcam" (video)
/// [dshow @ 000...] "麦克风 (Realtek)" (audio)
pub fn parse_dshow(text: &str) -> Vec<AvDevice> {
    let mut devs = Vec::new();
    for line in text.lines() {
        if !line.contains("(video)") {
            continue;
        }
        let Some(start) = line.find('"') else { continue };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('"') else { continue };
        let name = rest[..end].trim().to_string();
        if name.is_empty() || devs.iter().any(|d: &AvDevice| d.name == name) {
            continue;
        }
        let index = devs.len() as u32;
        devs.push(AvDevice { index, name });
    }
    devs
}

pub fn list_dshow() -> Vec<AvDevice> {
    let Some(ffmpeg) = ffmpeg::ffmpeg_path() else { return Vec::new() };
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"]);
    match ffmpeg::run_with_timeout(cmd, Duration::from_secs(6)) {
        Some(out) => parse_dshow(&String::from_utf8_lossy(&out.stderr)),
        None => Vec::new(),
    }
}

/// 按平台枚举相机：Windows 走 DirectShow，其它平台走 AVFoundation。
pub fn list_cameras() -> Vec<AvDevice> {
    if cfg!(windows) {
        list_dshow()
    } else {
        list_avfoundation()
    }
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
    fn parses_dshow_video_devices() {
        let text = "[dshow @ 0000020] \"HD Webcam\" (video)
                    [dshow @ 0000020]   Alternative name \"@device_pnp_\\\\?\\usb#vid\"
                    [dshow @ 0000020] \"麦克风 (Realtek Audio)\" (audio)
                    [dshow @ 0000020] \"OBS Virtual Camera\" (video)";
        let devs = parse_dshow(text);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].name, "HD Webcam");
        assert_eq!(devs[1].index, 1);
        assert_eq!(devs[1].name, "OBS Virtual Camera");
    }

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

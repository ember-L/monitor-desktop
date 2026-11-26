//! Android 设备枚举与 scrcpy-server 资产。
//! 视频流协议按 scrcpy v3.1 Streamer.java 实现（见 source.rs）。

use crate::ffmpeg;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// 内嵌的 scrcpy-server（Apache-2.0, (c) Genymobile），运行时推到设备。
pub const SCRCPY_SERVER: &[u8] = include_bytes!("../assets/scrcpy-server-v3.1");
pub const SCRCPY_VERSION: &str = "3.1";

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct AndroidDevice {
    pub serial: String,
    pub model: String,
}

pub fn adb_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MONITOR_ADB") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(out) = Command::new("which").arg("adb").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for p in [
        format!("{home}/Library/Android/sdk/platform-tools/adb"),
        "/opt/homebrew/bin/adb".to_string(),
        "/usr/local/bin/adb".to_string(),
    ] {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

pub fn parse_devices(text: &str) -> Vec<AndroidDevice> {
    let mut devs = Vec::new();
    for line in text.lines().skip(1) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 2 || toks[1] != "device" {
            continue;
        }
        let serial = toks[0].to_string();
        let model = toks
            .iter()
            .find_map(|t| t.strip_prefix("model:"))
            .unwrap_or(&serial)
            .to_string();
        devs.push(AndroidDevice { serial, model });
    }
    devs
}

pub fn list_devices() -> Vec<AndroidDevice> {
    let Some(adb) = adb_path() else { return Vec::new() };
    let mut cmd = Command::new(adb);
    cmd.args(["devices", "-l"]);
    match ffmpeg::run_with_timeout(cmd, Duration::from_secs(5)) {
        Some(out) => parse_devices(&String::from_utf8_lossy(&out.stdout)),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices() {
        let text = "List of devices attached
                    ABC123 device product:raven model:Pixel_6_Pro device:raven transport_id:3
                    XYZ offline
                    DEF456 unauthorized usb:1
";
        let devs = parse_devices(text);
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].serial, "ABC123");
        assert_eq!(devs[0].model, "Pixel_6_Pro");
    }

    #[test]
    fn server_asset_embedded() {
        assert!(SCRCPY_SERVER.len() > 50_000);
    }
}

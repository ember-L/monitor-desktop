//! 统一创建子进程。
//!
//! Windows 上 GUI 程序（本应用）调用控制台程序（ffmpeg、adb、ipconfig…）时，
//! 系统会为子进程新开一个控制台窗口 —— 用户看到 cmd 黑框闪一下又消失；
//! 设备列表每隔几秒刷新一次，黑框就会反复闪。CREATE_NO_WINDOW 关掉它。
//! macOS 上不需要任何处理。

use std::process::Command;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 建一条子进程命令：Windows 上不会弹出控制台窗口。
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

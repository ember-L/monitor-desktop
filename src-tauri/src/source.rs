//! 视频源管道：本机摄像头 / iPhone USB（ffmpeg avfoundation）、Android 屏幕
//! （内嵌 scrcpy-server 推流 + ffmpeg 解码）、手机无线（WSS 推流）。
//! 统一产出 **JPEG 帧**：预览走本地 MJPEG 服务，拍照直接落盘，录像时才解码为 RGB。

use crate::{android, devices, ffmpeg, wireless::Wireless};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum DeviceId {
    Av(u32),
    Android(String),
    Wireless,
}

impl DeviceId {
    /// 稳定标识（日志 / 去重用；界面直接传 kind + id）。
    #[allow(dead_code)]
    pub fn key(&self) -> String {
        match self {
            DeviceId::Av(i) => format!("av:{i}"),
            DeviceId::Android(s) => format!("android:{s}"),
            DeviceId::Wireless => "wireless".to_string(),
        }
    }

    pub fn parse(kind: &str, id: &str) -> Option<DeviceId> {
        match kind {
            "av" => id.parse().ok().map(DeviceId::Av),
            "android" => {
                if id.is_empty() {
                    None
                } else {
                    Some(DeviceId::Android(id.to_string()))
                }
            }
            "wireless" => Some(DeviceId::Wireless),
            _ => None,
        }
    }
}

/// 一帧 JPEG 画面（不在这里解码）。
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub jpeg: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceState {
    Starting,
    Live,
    Error(String),
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DeviceLists {
    pub av: Vec<devices::AvDevice>,
    pub android: Vec<android::AndroidDevice>,
    pub ffmpeg_ok: bool,
    pub adb_ok: bool,
}

/// 缩略图（JPEG）。宽高只用于自检与测试，界面直接渲染 JPEG。
#[derive(Clone)]
#[allow(dead_code)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// 后台 → 应用的事件。gen 用于丢弃已切换源的过期事件。
pub enum Event {
    Frame { gen: u64, frame: Frame },
    State { gen: u64, state: SourceState },
    Wireless { connected: bool, name: Option<String>, hint: Option<String> },
}

/// 枚举设备（阻塞，命令层放到阻塞线程池调用）。
pub fn enumerate_devices() -> DeviceLists {
    let ffmpeg_ok = ffmpeg::ffmpeg_path().is_some();
    let adb_ok = android::adb_path().is_some();
    let av = if ffmpeg_ok { devices::list_avfoundation() } else { Vec::new() };
    let android = if adb_ok { android::list_devices() } else { Vec::new() };
    DeviceLists { av, android, ffmpeg_ok, adb_ok }
}

pub struct SourceHandle {
    stop: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Child>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    cleanup: Option<Cleanup>,
    wireless: Option<Arc<Wireless>>,
}

struct Cleanup {
    adb: PathBuf,
    serial: String,
    port: u16,
    scid: String,
}

impl SourceHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = &self.wireless {
            w.clear_sink();
        }
        if let Ok(mut cs) = self.children.lock() {
            for c in cs.iter_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            cs.clear();
        }
        if let Ok(mut ts) = self.threads.lock() {
            for t in ts.drain(..) {
                let _ = t.join();
            }
        }
        if let Some(cl) = self.cleanup.take() {
            // 清理 adb 转发与设备端 server（不阻塞 UI）
            std::thread::spawn(move || {
                let mut cmd = Command::new(&cl.adb);
                cmd.args(["-s", &cl.serial, "forward", "--remove", &format!("tcp:{}", cl.port)]);
                let _ = ffmpeg::run_with_timeout(cmd, Duration::from_secs(3));
                let mut cmd = Command::new(&cl.adb);
                cmd.args(["-s", &cl.serial, "shell", &format!("pkill -f scid={}", cl.scid)]);
                let _ = ffmpeg::run_with_timeout(cmd, Duration::from_secs(3));
            });
        }
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 解析 scrcpy v3.1 视频头：codec id + 宽 + 高（各 4 字节大端）。
pub fn parse_video_header(hdr: &[u8]) -> Option<(u32, u32, u32)> {
    if hdr.len() < 12 {
        return None;
    }
    let codec = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let w = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let h = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    if codec != 0x6832_3634 {
        // 'h264'
        return None;
    }
    if !(2..=8192).contains(&w) || !(2..=8192).contains(&h) {
        return None;
    }
    Some((codec, w, h))
}

/// 读 ffmpeg 的 MJPEG 输出，切帧后直接转发 JPEG（不解码）。
fn pump_mjpeg(
    gen: u64,
    mut stdout: std::process::ChildStdout,
    stop: Arc<AtomicBool>,
    tx: Sender<Event>,
    sink: Arc<Mutex<Vec<String>>>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut chunk = [0u8; 65536];
    let mut live = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        let jpegs = ffmpeg::extract_jpegs(&mut buf);
        if jpegs.is_empty() {
            continue;
        }
        // 只送最新一帧，避免积压
        let jpg = jpegs.into_iter().last().unwrap();
        let Some((width, height)) = ffmpeg::jpeg_size(&jpg) else {
            continue;
        };
        if !live {
            live = true;
            let _ = tx.send(Event::State { gen, state: SourceState::Live });
        }
        let _ = tx.send(Event::Frame {
            gen,
            frame: Frame { width, height, jpeg: Arc::new(jpg) },
        });
        if buf.len() > 32 * 1024 * 1024 {
            buf.clear(); // 损坏流保护
        }
    }
    if stop.load(Ordering::SeqCst) {
        let _ = tx.send(Event::State { gen, state: SourceState::Stopped });
    } else {
        let detail = sink.lock().map(|s| s.join(" | ")).unwrap_or_default();
        let msg = if detail.is_empty() { "视频流已结束".to_string() } else { detail };
        let _ = tx.send(Event::State { gen, state: SourceState::Error(msg) });
    }
}

fn spawn_child(cmd: &mut Command) -> std::io::Result<Child> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    cmd.spawn()
}

/// 本机摄像头 / iPhone USB：avfoundation → MJPEG 管道。

/// 部分 macOS 摄像头不支持 avfoundation 默认的 yuv420p，
/// 先探测一帧能否成功编码，必要时改用 uyvy422 / nv12。
fn pick_pix_fmt(ff: &std::path::Path, index: u32) -> Result<Option<&'static str>, String> {
    let candidates: [Option<&'static str>; 3] = [None, Some("uyvy422"), Some("nv12")];
    let mut last_err = String::new();
    for cand in candidates {
        let mut cmd = Command::new(ff);
        cmd.args(["-hide_banner", "-loglevel", "error", "-f", "avfoundation", "-framerate", "30"]);
        if let Some(p) = cand {
            cmd.args(["-pix_fmt", p]);
        }
        cmd.args([
            "-i", &format!("{index}:none"),
            "-frames:v", "1", "-an", "-c:v", "mjpeg", "-q:v", "3",
            "-f", "mjpeg", "-y", "/dev/null",
        ]);
        let Some(out) = ffmpeg::run_with_timeout(cmd, Duration::from_secs(6)) else {
            return Err("相机没有响应：请在「系统设置 → 隐私与安全性 → 相机」中允许 MONITOR / 终端访问摄像头，并确认设备未被其他程序占用".to_string());
        };
        if out.status.success() {
            return Ok(cand);
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        last_err = if err.is_empty() { "相机无法输出画面".to_string() } else { err };
        let format_issue = last_err.contains("pixel format") || last_err.contains("not supported by the input device");
        if !format_issue {
            return Err(last_err);
        }
    }
    Err(if last_err.is_empty() { "相机不支持可用的像素格式".to_string() } else { last_err })
}

pub fn spawn_camera(gen: u64, index: u32, tx: Sender<Event>) -> Result<SourceHandle, String> {
    let ff = ffmpeg::ffmpeg_path().ok_or("未找到 ffmpeg，请先安装：brew install ffmpeg")?;
    let pix_fmt = pick_pix_fmt(&ff, index)?;
    let stop = Arc::new(AtomicBool::new(false));
    let children: Arc<Mutex<Vec<Child>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(Mutex::new(Vec::new()));
    let _ = tx.send(Event::State { gen, state: SourceState::Starting });

    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error",
        "-f", "avfoundation", "-framerate", "30",
    ]);
    if let Some(p) = pix_fmt {
        cmd.args(["-pix_fmt", p]);
    }
    cmd.args([
        "-i", &format!("{index}:none"),
        "-an", "-c:v", "mjpeg", "-q:v", "3",
        "-f", "image2pipe", "pipe:1",
    ]);
    let mut child = spawn_child(&mut cmd).map_err(|e| format!("启动 ffmpeg 失败：{e}"))?;
    let stdout = child.stdout.take().ok_or("无法获取 ffmpeg 输出")?;
    if let Some(err) = child.stderr.take() {
        ffmpeg::drain_stderr(err, sink.clone());
    }
    children.lock().unwrap().push(child);

    let mut threads = Vec::new();
    {
        let (stop, tx, sink) = (stop.clone(), tx.clone(), sink.clone());
        threads.push(std::thread::spawn(move || pump_mjpeg(gen, stdout, stop, tx, sink)));
    }
    Ok(SourceHandle { stop, children, threads: Mutex::new(threads), cleanup: None, wireless: None })
}

/// Android 屏幕：adb 推 scrcpy-server → forward → H.264 → ffmpeg → MJPEG 管道。
pub fn spawn_android(gen: u64, serial: &str, tx: Sender<Event>) -> Result<SourceHandle, String> {
    let adb = android::adb_path().ok_or("未找到 adb，请安装 Android platform-tools")?;
    let ff = ffmpeg::ffmpeg_path().ok_or("未找到 ffmpeg，请先安装：brew install ffmpeg")?;
    let stop = Arc::new(AtomicBool::new(false));
    let children: Arc<Mutex<Vec<Child>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(Mutex::new(Vec::new()));
    let _ = tx.send(Event::State { gen, state: SourceState::Starting });

    let fail = |msg: String| -> String {
        let _ = tx.send(Event::State { gen, state: SourceState::Error(msg.clone()) });
        msg
    };

    // 1. 推送内嵌 server
    let jar = std::env::temp_dir().join(format!("monitor-scrcpy-server-{}.jar", std::process::id()));
    std::fs::write(&jar, android::SCRCPY_SERVER).map_err(|e| fail(format!("写入临时 server 失败：{e}")))?;
    let mut cmd = Command::new(&adb);
    cmd.args(["-s", serial, "push", jar.to_str().unwrap_or_default(), "/data/local/tmp/scrcpy-server.jar"]);
    let out = ffmpeg::run_with_timeout(cmd, Duration::from_secs(20)).ok_or_else(|| fail("adb push 超时".into()))?;
    if !out.status.success() {
        return Err(fail(format!("adb push 失败：{}", String::from_utf8_lossy(&out.stderr).trim())));
    }

    // 2. 随机 scid，避免与正在运行的 scrcpy 冲突；tcp:0 让 adb 分配空闲端口
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    let scid = format!("{:08x}", nanos & 0x7fff_ffff);
    let mut cmd = Command::new(&adb);
    cmd.args(["-s", serial, "forward", "tcp:0", &format!("localabstract:scrcpy_{scid}")]);
    let out = ffmpeg::run_with_timeout(cmd, Duration::from_secs(5)).ok_or_else(|| fail("adb forward 超时".into()))?;
    let port: u16 = String::from_utf8_lossy(&out.stdout).trim().parse()
        .map_err(|_| fail("无法解析 adb 转发端口".into()))?;

    let cleanup = Some(Cleanup { adb: adb.clone(), serial: serial.to_string(), port, scid: scid.clone() });
    let mut handle = SourceHandle { stop: stop.clone(), children: children.clone(), threads: Mutex::new(Vec::new()), cleanup, wireless: None };

    // 3. 启动 server（forward 模式：server 监听等待连接）
    let mut cmd = Command::new(&adb);
    cmd.args([
        "-s", serial, "shell",
        "CLASSPATH=/data/local/tmp/scrcpy-server.jar",
        "app_process", "/", "com.genymobile.scrcpy.Server",
        android::SCRCPY_VERSION,
        "tunnel_forward=true", "video=true", "audio=false", "control=false",
        &format!("scid={scid}"),
        "max_size=1280", "max_fps=30", "video_bit_rate=8000000",
        "send_frame_meta=false", "send_dummy_byte=false", "send_device_meta=false",
    ]);
    let mut server = spawn_child(&mut cmd).map_err(|e| fail(format!("启动 adb shell 失败：{e}")))?;
    if let Some(err) = server.stderr.take() {
        ffmpeg::drain_stderr(err, sink.clone());
    }
    if let Some(out) = server.stdout.take() {
        ffmpeg::drain_stderr(out, sink.clone());
    }
    children.lock().unwrap().push(server);

    // 4. 连接视频 socket（server 启动需要一两秒）
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut sock = None;
    while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => { sock = Some(s); break; }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let mut sock = match sock {
        Some(s) => s,
        None => {
            handle.stop();
            return Err(fail("连接设备屏幕流失败（请确认已授权 USB 调试）".into()));
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));

    // 5. 12 字节视频头（codec id / 宽 / 高，大端）
    let mut hdr = [0u8; 12];
    if sock.read_exact(&mut hdr).is_err() {
        handle.stop();
        return Err(fail("设备未发送视频头（server 可能启动失败，见状态栏诊断）".into()));
    }
    if parse_video_header(&hdr).is_none() {
        handle.stop();
        return Err(fail("视频头无效（编码不是 H.264 或尺寸异常）".into()));
    }

    // 6. ffmpeg 解码 H.264 → MJPEG 管道
    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error",
        "-f", "h264", "-probesize", "32", "-analyzeduration", "0",
        "-i", "pipe:0",
        "-an", "-c:v", "mjpeg", "-q:v", "3",
        "-f", "image2pipe", "pipe:1",
    ]);
    cmd.stdin(Stdio::piped());
    let mut dec = spawn_child(&mut cmd).map_err(|e| fail(format!("启动解码器失败：{e}")))?;
    let mut dec_stdin = dec.stdin.take().ok_or_else(|| fail("无法获取解码器输入".into()))?;
    let dec_stdout = dec.stdout.take().ok_or_else(|| fail("无法获取解码器输出".into()))?;
    if let Some(err) = dec.stderr.take() {
        ffmpeg::drain_stderr(err, sink.clone());
    }
    children.lock().unwrap().push(dec);

    // 写线程：socket → ffmpeg stdin
    {
        let stop = stop.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut chunk = [0u8; 65536];
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match sock.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if dec_stdin.write_all(&chunk[..n]).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(_) => break,
                }
            }
        });
        handle.threads.lock().unwrap().push(writer);
    }
    // 读线程：MJPEG → JPEG 帧
    {
        let (stop, tx, sink) = (stop.clone(), tx.clone(), sink.clone());
        let reader = std::thread::spawn(move || pump_mjpeg(gen, dec_stdout, stop, tx, sink));
        handle.threads.lock().unwrap().push(reader);
    }
    Ok(handle)
}

/// 手机无线：WSS 推送的 JPEG 帧 → 统一的 Frame 事件。
pub fn spawn_wireless(gen: u64, tx: Sender<Event>, wireless: Arc<Wireless>) -> Result<SourceHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let rx: Receiver<Vec<u8>> = wireless.subscribe();
    let _ = tx.send(Event::State { gen, state: SourceState::Starting });
    let handle = SourceHandle {
        stop: stop.clone(),
        children: Arc::new(Mutex::new(Vec::new())),
        threads: Mutex::new(Vec::new()),
        cleanup: None,
        wireless: Some(wireless.clone()),
    };
    let (s, t) = (stop.clone(), tx.clone());
    let reader = std::thread::spawn(move || {
        let mut live = false;
        loop {
            if s.load(Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(jpeg) => {
                    let Some((width, height)) = ffmpeg::jpeg_size(&jpeg) else { continue };
                    if !live {
                        live = true;
                        let _ = t.send(Event::State { gen, state: SourceState::Live });
                    }
                    let _ = t.send(Event::Frame { gen, frame: Frame { width, height, jpeg: Arc::new(jpeg) } });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        let _ = t.send(Event::State { gen, state: SourceState::Stopped });
    });
    handle.threads.lock().unwrap().push(reader);
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(codec: u32, w: u32, h: u32) -> Vec<u8> {
        let mut v = codec.to_be_bytes().to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn parses_valid_h264_header() {
        let got = parse_video_header(&hdr(0x6832_3634, 1280, 720));
        assert_eq!(got, Some((0x6832_3634, 1280, 720)));
    }

    #[test]
    fn rejects_wrong_codec_or_size() {
        assert!(parse_video_header(&hdr(0x6832_3635, 1280, 720)).is_none()); // 'h265'
        assert!(parse_video_header(&hdr(0x6832_3634, 0, 720)).is_none());
        assert!(parse_video_header(&hdr(0x6832_3634, 1280, 99999)).is_none());
        assert!(parse_video_header(&[0; 8]).is_none());
    }

    #[test]
    fn parses_device_ids() {
        assert_eq!(DeviceId::parse("av", "2"), Some(DeviceId::Av(2)));
        assert_eq!(DeviceId::parse("android", "ABC"), Some(DeviceId::Android("ABC".into())));
        assert_eq!(DeviceId::parse("wireless", "phone"), Some(DeviceId::Wireless));
        assert_eq!(DeviceId::parse("av", "x"), None);
        assert_eq!(DeviceId::parse("android", ""), None);
    }
}

//! 视频源管道：本机摄像头 / iPhone USB（ffmpeg avfoundation）与 Android 屏幕
//! （内嵌 scrcpy-server 推流 + ffmpeg 解码）。统一输出 MJPEG 管道帧，解码为 RGB。

use crate::{android, devices, ffmpeg};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum DeviceId {
    Av(u32),
    Android(String),
}

/// 一帧 RGB 画面。
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceState {
    Starting,
    Live,
    Error(String),
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceLists {
    pub av: Vec<devices::AvDevice>,
    pub android: Vec<android::AndroidDevice>,
    pub ffmpeg_ok: bool,
    pub adb_ok: bool,
}

/// 缩略图（RGB）。
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// 所有后台线程 → UI 的事件。gen 用于丢弃已切换源的过期事件。
pub enum Event {
    Frame { gen: u64, frame: Frame },
    State { gen: u64, state: SourceState },
    Fps { gen: u64, fps: f32 },
    SourceReady { gen: u64, result: Result<SourceHandle, String> },
    Devices(DeviceLists),
    PhotoSaved { path: PathBuf, thumb: Thumb },
    RecordingDone { path: PathBuf, thumb: Option<Thumb>, secs: f64, error: Option<String> },
    Notice(String),
}

/// 枚举设备（后台线程调用）。
pub fn enumerate_devices(tx: Sender<Event>) {
    let ffmpeg_ok = ffmpeg::ffmpeg_path().is_some();
    let adb_ok = android::adb_path().is_some();
    let av = devices::list_avfoundation();
    let android = android::list_devices();
    let _ = tx.send(Event::Devices(DeviceLists { av, android, ffmpeg_ok, adb_ok }));
}

pub struct SourceHandle {
    stop: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Child>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    cleanup: Option<Cleanup>,
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

/// 读 ffmpeg 的 MJPEG 输出，解码并发送帧；统计 FPS。
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
    let mut frames = 0u32;
    let mut t0 = Instant::now();
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
        frames += jpegs.len() as u32;
        // 只解码最新一帧，避免积压
        let jpg = jpegs.last().unwrap();
        let Ok(img) = image::load_from_memory_with_format(jpg, image::ImageFormat::Jpeg) else {
            continue;
        };
        let rgb = img.to_rgb8();
        let (width, height) = rgb.dimensions();
        if !live {
            live = true;
            let _ = tx.send(Event::State { gen, state: SourceState::Live });
        }
        let _ = tx.send(Event::Frame { gen, frame: Frame { width, height, rgb: Arc::new(rgb.into_raw()) } });
        if t0.elapsed() >= Duration::from_secs(1) {
            let fps = frames as f32 / t0.elapsed().as_secs_f32();
            let _ = tx.send(Event::Fps { gen, fps });
            frames = 0;
            t0 = Instant::now();
        }
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
pub fn spawn_camera(gen: u64, index: u32, tx: Sender<Event>) -> Result<SourceHandle, String> {
    let ff = ffmpeg::ffmpeg_path().ok_or("未找到 ffmpeg，请先安装：brew install ffmpeg")?;
    let stop = Arc::new(AtomicBool::new(false));
    let children: Arc<Mutex<Vec<Child>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(Mutex::new(Vec::new()));
    let _ = tx.send(Event::State { gen, state: SourceState::Starting });

    let mut cmd = Command::new(ff);
    cmd.args([
        "-hide_banner", "-loglevel", "error",
        "-f", "avfoundation", "-framerate", "30",
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
    Ok(SourceHandle { stop, children, threads: Mutex::new(threads), cleanup: None })
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
    let jar = std::env::temp_dir().join("monitor-scrcpy-server.jar");
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
    let mut handle = SourceHandle { stop: stop.clone(), children: children.clone(), threads: Mutex::new(Vec::new()), cleanup };

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
    // 读超时：让写线程能周期性检查 stop 标志，避免停止时 join 卡死
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));

    // 5. 12 字节视频头（codec id / 宽 / 高，大端）
    let mut hdr = [0u8; 12];
    if sock.read_exact(&mut hdr).is_err() {
        handle.stop();
        return Err(fail("设备未发送视频头（server 可能启动失败，见状态栏诊断）".into()));
    }
    let Some((_codec, _w, _h)) = parse_video_header(&hdr) else {
        handle.stop();
        return Err(fail("视频头无效（编码不是 H.264 或尺寸异常）".into()));
    };

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
            // 关闭 stdin 让 ffmpeg 收尾退出
        });
        handle.threads.lock().unwrap().push(writer);
    }
    // 读线程：MJPEG → RGB 帧
    {
        let (stop, tx, sink) = (stop.clone(), tx.clone(), sink.clone());
        let reader = std::thread::spawn(move || pump_mjpeg(gen, dec_stdout, stop, tx, sink));
        handle.threads.lock().unwrap().push(reader);
    }
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
}

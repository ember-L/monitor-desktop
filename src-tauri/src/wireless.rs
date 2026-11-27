//! 手机无线推流：局域网 HTTPS 提供手机页，WSS 接收手机摄像头 JPEG 帧。
//! 自签证书（rcgen + rustls ring），端口随机，配对靠 URL 里的随机 token。

use crate::http::{self, WsConn, WsMsg};
use crate::source::Event;
use serde::Serialize;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const PHONE_PAGE: &str = include_str!("phone_page.html");

/// 单条连接最多处理的请求数（逐帧上行会长期复用同一条连接）。
/// 24 fps 下 8192 只够 5 分钟，用满就断连、重连会掉帧；放宽到约 45 分钟。
const MAX_REQUESTS_PER_CONN: usize = 65536;

/// 配对信息（前端展示二维码与地址）。
#[derive(Clone, Serialize)]
pub struct PairInfo {
    pub url: String,
    pub lan: String,
    /// 本机候选局域网地址（Wi‑Fi 优先），供界面切换。
    pub ips: Vec<String>,
    pub port: u16,
    pub token: String,
    pub available: bool,
    pub hint: String,
    #[serde(skip)]
    pub qr: Vec<u8>,
}

/// 连接诊断计数：界面据此区分「手机没连上」和「连上了没推流」。
#[derive(Default)]
struct Diag {
    accepts: u64,
    requests: u64,
    page_loads: u64,
    frame_posts: u64,
    ws_frames: u64,
    bytes: u64,
    errors: u64,
    last_peer: String,
    last_request: String,
    last_error: String,
    last_at: String,
}

fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 帧槽状态：最新一帧 + 版本号。生产者覆盖写，消费者按版本号取走。
struct SlotState {
    latest: Option<Vec<u8>>,
    seq: u64,
    closed: bool,
    dropped: u64,
}

/// 无线推流只保留最新一帧。
///
/// 之前用无界 mpsc：桌面端一旦读得慢（解码、截图、切后台），队列会无限堆积，
/// 画面就越来越滞后，甚至几十秒后才显示「刚才」的画面。实时取景里旧帧没有价值，
/// 所以改成覆盖式帧槽：生产者永远写最新帧，消费者永远拿最新帧，队列长度恒为 1。
struct FrameSlot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState { latest: None, seq: 0, closed: false, dropped: 0 }),
            cv: Condvar::new(),
        }
    }

    fn push(&self, data: Vec<u8>) {
        if let Ok(mut s) = self.state.lock() {
            if s.latest.is_some() {
                // 上一帧还没被取走：直接覆盖，宁可丢帧也不让画面落后。
                s.dropped += 1;
            }
            s.latest = Some(data);
            s.seq = s.seq.wrapping_add(1);
        }
        self.cv.notify_all();
    }

    fn close(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.closed = true;
        }
        self.cv.notify_all();
    }

    fn wait(&self, seen: &mut u64, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let Ok(mut s) = self.state.lock() else {
            return Err(RecvTimeoutError::Disconnected);
        };
        loop {
            if s.seq != *seen {
                *seen = s.seq;
                if let Some(data) = s.latest.take() {
                    return Ok(data);
                }
            }
            if s.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
            match self.cv.wait_timeout(s, deadline - now) {
                Ok((guard, _)) => s = guard,
                Err(_) => return Err(RecvTimeoutError::Disconnected),
            }
        }
    }
}

/// 订阅端：与 mpsc::Receiver::recv_timeout 同形，方便调用方平滑迁移。
pub struct FrameRx {
    slot: Arc<FrameSlot>,
    seen: u64,
}

impl FrameRx {
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        self.slot.wait(&mut self.seen, timeout)
    }
}

pub struct Wireless {
    tx: Sender<Event>,
    sink: Mutex<Option<Arc<FrameSlot>>>,
    /// 还没有订阅者时（桌面还没开源）缓存的最新一帧：sink 一出现就清空，从不排队。
    /// 没有订阅者时保留的最新一帧（手机先连上、桌面后开源时立刻有画面）。
    latest: Mutex<Option<Vec<u8>>>,
    /// 本机自签证书 DER，供手机从 /cert 下载安装。
    cert: Mutex<Vec<u8>>,
    /// 连接与请求计数（诊断用，不含画面内容）。
    diag: Mutex<Diag>,
    info: Mutex<Option<PairInfo>>,
    connected: Mutex<Option<String>>,
    stop: Arc<AtomicBool>,
    starting: Mutex<bool>,
}

impl Wireless {
    pub fn new(tx: Sender<Event>) -> Arc<Self> {
        Arc::new(Self {
            tx,
            sink: Mutex::new(None),
            latest: Mutex::new(None),
            cert: Mutex::new(Vec::new()),
            diag: Mutex::new(Diag::default()),
            info: Mutex::new(None),
            connected: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            starting: Mutex::new(false),
        })
    }

    fn unavailable(hint: String) -> PairInfo {
        PairInfo {
            url: String::new(),
            lan: String::new(),
            ips: Vec::new(),
            port: 0,
            token: String::new(),
            available: false,
            hint,
            qr: Vec::new(),
        }
    }

    /// 首次调用启动服务；已启动则直接返回缓存信息。
    pub fn ensure_started(self: &Arc<Self>) -> PairInfo {
        if let Ok(info) = self.info.lock() {
            if let Some(i) = info.as_ref() {
                if !self.stop.load(Ordering::SeqCst) {
                    return i.clone();
                }
            }
        }
        let mut starting = match self.starting.lock() {
            Ok(g) => g,
            Err(_) => return Self::unavailable("无线服务状态异常".into()),
        };
        if *starting {
            drop(starting);
            std::thread::sleep(Duration::from_millis(200));
            return self.info.lock().ok().and_then(|i| i.clone()).unwrap_or_else(|| Self::unavailable("无线服务启动中".into()));
        }
        *starting = true;
        drop(starting);
        let result = self.start_server();
        if let Ok(mut s) = self.starting.lock() {
            *s = false;
        }
        if let Ok(mut info) = self.info.lock() {
            *info = Some(result.clone());
        }
        result
    }

    fn start_server(self: &Arc<Self>) -> PairInfo {
        let ips = lan_ips();
        let Some(lan) = ips.first().cloned() else {
            return Self::unavailable("未连接局域网，请接入 Wi-Fi 或有线网络后重试".into());
        };
        let token = random_token();
        let (config, cert_der) = match tls_config(&lan) {
            Ok(c) => c,
            Err(e) => return Self::unavailable(format!("生成自签证书失败：{e}")),
        };
        if let Ok(mut c) = self.cert.lock() {
            *c = cert_der;
        }
        let listener = match TcpListener::bind(("0.0.0.0", 0)) {
            Ok(l) => l,
            Err(e) => return Self::unavailable(format!("无法监听无线端口：{e}")),
        };
        let port = match listener.local_addr() {
            Ok(a) => a.port(),
            Err(e) => return Self::unavailable(format!("获取无线端口失败：{e}")),
        };
        self.stop.store(false, Ordering::SeqCst);

        let url = format!("https://{lan}:{port}/?t={token}");
        let qr = make_qr_png(&url).unwrap_or_default();

        let me = self.clone();
        let cfg = config;
        let stop = self.stop.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let w = me.clone();
                let cfg = cfg.clone();
                std::thread::spawn(move || serve_conn(w, cfg, stream));
            }
        });

        PairInfo {
            url,
            lan,
            ips,
            port,
            token,
            available: true,
            hint: "手机与电脑需在同一 Wi-Fi；首次打开会提示证书不受信任，选择继续访问即可。".to_string(),
            qr,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let port = self.info.lock().ok().and_then(|i| i.as_ref().map(|p| p.port)).unwrap_or(0);
        if port != 0 {
            // 唤醒阻塞在 accept 的监听线程
            let _ = TcpStream::connect(("127.0.0.1", port));
        }
        if let Ok(mut i) = self.info.lock() {
            *i = None;
        }
        if let Ok(mut c) = self.connected.lock() {
            *c = None;
        }
    }

    #[allow(dead_code)]
    pub fn info(&self) -> Option<PairInfo> {
        self.info.lock().ok().and_then(|i| i.clone())
    }

    /// 重新读取本机 IP 并刷新配对地址与二维码（服务端口与 token 不变）。
    /// `preferred` 指定候选地址时，仅在它仍是本机现存地址时才采用。
    pub fn refresh(self: &Arc<Self>, preferred: Option<&str>) -> PairInfo {
        let base = self.ensure_started();
        if !base.available {
            return base;
        }
        let ips = lan_ips();
        if ips.is_empty() {
            return base;
        }
        let lan = match preferred {
            Some(p) if ips.iter().any(|ip| ip == p) => p.to_string(),
            _ if ips.iter().any(|ip| *ip == base.lan) => base.lan.clone(),
            _ => ips[0].clone(),
        };
        let url = format!("https://{lan}:{}/?t={}", base.port, base.token);
        let qr = make_qr_png(&url).unwrap_or_default();
        let updated = PairInfo {
            url,
            lan,
            ips,
            port: base.port,
            token: base.token.clone(),
            available: true,
            hint: base.hint.clone(),
            qr,
        };
        if let Ok(mut slot) = self.info.lock() {
            *slot = Some(updated.clone());
        }
        updated
    }

    /// 订阅帧。之前没有订阅者时，把缓冲的最新一帧一并返回，桌面立刻能出画面。
    pub fn subscribe(&self) -> (FrameRx, Option<Vec<u8>>) {
        let slot = Arc::new(FrameSlot::new());
        let pending = self.latest.lock().ok().and_then(|mut l| l.take());
        if let Ok(mut sink) = self.sink.lock() {
            if let Some(old) = sink.replace(slot.clone()) {
                // 换订阅者（重新开源）：旧槽里的等待线程要能退出。
                old.close();
            }
        }
        (FrameRx { slot, seen: 0 }, pending)
    }

    pub fn clear_sink(&self) {
        if let Ok(mut sink) = self.sink.lock() {
            if let Some(slot) = sink.take() {
                slot.close();
            }
        }
        if let Ok(mut latest) = self.latest.lock() {
            *latest = None;
        }
    }

    /// 手机推来的每一帧都走这里：直接覆盖帧槽里的旧帧，绝不排队。
    fn push_frame(&self, data: Vec<u8>) {
        let slot = self.sink.lock().ok().and_then(|s| s.clone());
        if let Some(slot) = slot {
            slot.push(data);
            return;
        }
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(data);
        }
    }

    /// 逐帧 POST 上行没有握手，第一帧到达即视为手机上线。
    fn mark_connected(&self, name: Option<&str>) {
        let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let changed = match self.connected.lock() {
            Ok(mut c) => {
                if c.is_some() && name.is_none() {
                    false
                } else {
                    *c = name.clone();
                    true
                }
            }
            Err(_) => false,
        };
        if changed {
            let _ = self.tx.send(Event::Wireless { connected: true, name, hint: None });
        }
    }

    fn mark_disconnected(&self, hint: Option<String>) {
        let name = self.connected.lock().ok().and_then(|mut c| c.take());
        let _ = self.tx.send(Event::Wireless { connected: false, name, hint });
    }

    fn token(&self) -> String {
        self.info.lock().ok().and_then(|i| i.as_ref().map(|p| p.token.clone())).unwrap_or_default()
    }
}

fn tls_config(lan: &str) -> Result<(Arc<rustls::ServerConfig>, Vec<u8>), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key_pair = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(vec![lan.to_string(), "localhost".to_string()])
        .map_err(|e| e.to_string())?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "MONITOR 取景");
    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;
    let certs = vec![rustls::pki_types::CertificateDer::from(cert.der().to_vec())];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
    );
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    // 同一份证书也用于 /cert 下载：手机安装信任后 WSS 不再被拦。
    Ok((Arc::new(config), cert.der().to_vec()))
}

/// 本机候选局域网 IPv4 地址（Wi‑Fi 优先）。只取系统网卡里的私有/普通地址，
/// 排除回环、链路本地与 VPN、AWDL 等手机不可达的接口。
pub fn lan_ips() -> Vec<String> {
    let list = lan_ips_from_system();
    if !list.is_empty() {
        return list;
    }
    udp_lan_ip().into_iter().collect()
}

/// macOS / Linux：解析 `ifconfig`。
#[cfg(not(target_os = "windows"))]
fn lan_ips_from_system() -> Vec<String> {
    if let Ok(out) = crate::proc::command("ifconfig").output() {
        if out.status.success() {
            return lan_ips_from(&String::from_utf8_lossy(&out.stdout));
        }
    }
    Vec::new()
}

/// Windows：解析 `ipconfig`（中文/英文系统都带 "IPv4" 关键字）。
#[cfg(target_os = "windows")]
fn lan_ips_from_system() -> Vec<String> {
    if let Ok(out) = crate::proc::command("ipconfig").output() {
        if out.status.success() {
            return lan_ips_from_ipconfig(&String::from_utf8_lossy(&out.stdout));
        }
    }
    Vec::new()
}

/// 形如 `IPv4 地址 . . . . . . . . : 192.168.1.7` 或 `IPv4 Address. . . : 10.0.0.23(Preferred)`。
#[cfg(target_os = "windows")]
fn lan_ips_from_ipconfig(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        if !line.contains("IPv4") {
            continue;
        }
        let Some((_, rest)) = line.rsplit_once(':') else { continue };
        let token = rest.trim();
        let ip = token.split('(').next().unwrap_or("").trim();
        if is_lan_v4(ip) && !found.iter().any(|s| s == ip) {
            found.push(ip.to_string());
        }
    }
    found
}

#[cfg(not(target_os = "windows"))]
const SKIP_IFACE: [&str; 8] = ["lo", "utun", "awdl", "llw", "gif", "stf", "ap", "anpi"];

#[cfg(not(target_os = "windows"))]
fn lan_ips_from(text: &str) -> Vec<String> {
    let mut found: Vec<(u8, String, String)> = Vec::new();
    let mut iface = String::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            iface = line.split(':').next().unwrap_or("").trim().to_string();
            continue;
        }
        let Some(rest) = line.trim().strip_prefix("inet ") else { continue };
        let ip = rest.split_whitespace().next().unwrap_or("");
        if iface.is_empty() || !is_lan_v4(ip) || SKIP_IFACE.iter().any(|p| iface.starts_with(p)) {
            continue;
        }
        let prio = if iface == "en0" {
            0
        } else if iface.starts_with("en") {
            1
        } else if iface.starts_with("bridge") {
            2
        } else {
            3
        };
        if !found.iter().any(|(_, _, seen)| seen == ip) {
            found.push((prio, iface.clone(), ip.to_string()));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    found.into_iter().map(|(_, _, ip)| ip).collect()
}

fn is_lan_v4(ip: &str) -> bool {
    match ip.parse::<std::net::Ipv4Addr>() {
        Ok(addr) => {
            !addr.is_loopback()
                && !addr.is_unspecified()
                && !addr.is_link_local()
                && !addr.is_broadcast()
                && !addr.is_multicast()
        }
        Err(_) => false,
    }
}

/// 兜底：通过 UDP connect 探测出口网卡地址（不会真的发包）。
fn udp_lan_ip() -> Option<String> {
    for target in ["8.8.8.8:80", "1.1.1.1:80"] {
        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
            if sock.connect(target).is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    let ip = addr.ip();
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        return Some(ip.to_string());
                    }
                }
            }
        }
    }
    None
}

fn random_token() -> String {
    use std::hash::{Hash, Hasher};
    // Unix 上直接读内核随机数；Windows 没有 /dev/urandom，用时间 + 进程 + 地址混合。
    #[cfg(not(target_os = "windows"))]
    {
        use std::io::Read;
        let mut bytes = [0u8; 8];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .is_ok()
        {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    nanos.hash(&mut h);
    std::process::id().hash(&mut h);
    (&nanos as *const u128 as usize).hash(&mut h);
    let v = h.finish();
    format!("{nanos:016x}{v:016x}").chars().take(16).collect()
}

fn make_qr_png(url: &str) -> Option<Vec<u8>> {
    use image::Luma;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let img = code
        .render::<Luma<u8>>()
        .min_dimensions(420, 420)
        .quiet_zone(true)
        .build();
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}

/// 配对/连接诊断日志：写 ~/Library/Logs/MONITOR/wireless.log，写不进去时退到 /tmp/monitor-wireless.log。
/// 只记连接与请求行（不含 token），不记录画面内容。
fn trace(line: &str) {
    #[cfg(debug_assertions)]
    eprintln!("[wireless] {line}");
    let line = format!("{} {line}", now_stamp());
    let mut done = false;
    if let Some(home) = std::env::var_os("HOME") {
        let dir = std::path::PathBuf::from(home).join("Library/Logs/MONITOR");
        let _ = std::fs::create_dir_all(&dir);
        done = append_log(&dir.join("wireless.log"), &line);
    }
    if !done {
        let _ = append_log(std::path::Path::new("/tmp/monitor-wireless.log"), &line);
    }
}

/// 追加一行；文件超过 512KB 先删掉重来。写入成功返回 true。
fn append_log(path: &std::path::Path, line: &str) -> bool {
    use std::io::Write;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 512 * 1024 {
            let _ = std::fs::remove_file(path);
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => f.write_all(format!("{line}\n").as_bytes()).is_ok(),
        Err(_) => false,
    }
}

impl Wireless {
    fn with_diag(&self, f: impl FnOnce(&mut Diag)) {
        if let Ok(mut d) = self.diag.lock() {
            f(&mut d);
        }
    }

    fn note_accept(&self, peer: &str) {
        self.with_diag(|d| {
            d.accepts += 1;
            d.last_peer = peer.to_string();
            d.last_at = now_stamp();
        });
    }

    fn note_request(&self, method: &str, path: &str) {
        self.with_diag(|d| {
            d.requests += 1;
            d.last_request = format!("{method} {path}");
            d.last_at = now_stamp();
        });
    }

    fn note_page_load(&self) {
        self.with_diag(|d| d.page_loads += 1);
    }

    fn note_frame(&self, bytes: usize, ws: bool) {
        self.with_diag(|d| {
            if ws {
                d.ws_frames += 1;
            } else {
                d.frame_posts += 1;
            }
            d.bytes += bytes as u64;
        });
    }

    fn note_error(&self, msg: &str) {
        self.with_diag(|d| {
            d.errors += 1;
            d.last_error = msg.to_string();
        });
        trace(&format!("error: {msg}"));
    }

    /// 给界面看的实时诊断：手机连没连上、页面开没开、画面推没推过来。
    pub fn diag(&self) -> serde_json::Value {
        let guard = self.diag.lock().ok();
        let fallback = Diag::default();
        let d = guard.as_deref().unwrap_or(&fallback);
        let connected = self.connected.lock().ok().and_then(|c| c.clone());
        let buffered = self.latest.lock().map(|l| l.is_some()).unwrap_or(false);
        serde_json::json!({
            "connected": connected.is_some(),
            "connectedName": connected,
            "hasFrame": d.frame_posts + d.ws_frames > 0,
            "buffered": buffered,
            "accepts": d.accepts,
            "requests": d.requests,
            "pageLoads": d.page_loads,
            "framePosts": d.frame_posts,
            "wsFrames": d.ws_frames,
            "bytes": d.bytes,
            "errors": d.errors,
            "lastPeer": d.last_peer,
            "lastRequest": d.last_request,
            "lastError": d.last_error,
            "lastAt": d.last_at,
        })
    }
}

fn serve_conn(w: Arc<Wireless>, cfg: Arc<rustls::ServerConfig>, stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".to_string());
    trace(&format!("accept {peer}"));
    w.note_accept(&peer);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
    let tls = rustls::StreamOwned::new(conn, stream);
    let mut rd = BufReader::new(tls);
    // 一条 TLS 连接上连续处理多个请求：手机用 POST /frame 逐帧上行时复用它，
    // 避免每帧重新握手（这也是 WSS 被拦时的兜底通道）。
    let mut served: u32 = 0;
    for _ in 0..MAX_REQUESTS_PER_CONN {
        let req = match http::read_request(&mut rd) {
            Ok(r) => r,
            Err(e) => {
                trace(&format!("{peer} closed after {served} requests: {e}"));
                return;
            }
        };
        let path = req.path.clone();
        served += 1;
        w.note_request(&req.method, &path);
        if served <= 2 {
            trace(&format!("{peer} {} {path}", req.method));
        }

        if path == "/ws" && req.is_websocket_upgrade() {
            let Some(key) = req.header("sec-websocket-key").map(|s| s.to_string()) else {
                let _ = http::write_status(rd.get_mut(), 400, "Bad Request");
                return;
            };
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                http::ws_accept_key(&key)
            );
            if rd.get_mut().write_all(resp.as_bytes()).is_err() {
                return;
            }
            let _ = rd.get_mut().flush();
            ws_loop(w, WsConn::with_reader(rd));
            return;
        }

        // 逐帧上行兜底：WSS 因证书/网络环境被拦时，手机改用同源 HTTPS POST 推帧。
        if path == "/frame" && req.method == "POST" {
            if req.query_param("t").as_deref() != Some(w.token().as_str()) {
                w.note_error("配对码无效：手机推流被拒");
                let _ = http::write_status(rd.get_mut(), 403, "Forbidden");
                return;
            }
            let body = match http::read_body(&mut rd, http::content_length(&req)) {
                Ok(b) => b,
                Err(_) => return,
            };
            if body.len() < 4 || body[0] != 0xFF || body[1] != 0xD8 {
                let _ = http::write_status(rd.get_mut(), 400, "Bad Request");
                return;
            }
            let name = req.query_param("name");
            let len = body.len();
            w.mark_connected(name.as_deref());
            w.push_frame(body);
            w.note_frame(len, false);
            if http::write_response_keep_alive(rd.get_mut(), 204, "No Content", "text/plain; charset=utf-8", b"").is_err() {
                return;
            }
            continue;
        }

        // 手机离开页面：撤销「已连接」，让界面回到等待扫码。
        if path == "/bye" {
            if req.query_param("t").as_deref() != Some(w.token().as_str()) {
                let _ = http::write_status(rd.get_mut(), 403, "Forbidden");
                return;
            }
            w.mark_disconnected(Some("手机已断开连接".into()));
            let _ = http::write_response_keep_alive(rd.get_mut(), 204, "No Content", "text/plain; charset=utf-8", b"");
            continue;
        }

        // 手机页与图标：需要正确配对 token
        if path == "/" || path == "/index.html" {
            let token_ok = req.query_param("t").as_deref() == Some(w.token().as_str());
            if !token_ok {
                w.note_error("配对码无效：手机打开的是旧二维码地址");
                let _ = http::write_response(
                    rd.get_mut(),
                    403,
                    "Forbidden",
                    "text/html; charset=utf-8",
                    &[],
                    "<!doctype html><meta charset=utf-8><body style='font:16px -apple-system;background:#08090A;color:#E9EBE4;padding:40px'>配对码无效，请重新扫描电脑上的二维码。</body>".as_bytes(),
                );
                return;
            }
            // keep-alive：手机页之后的同源取帧请求可以复用这条连接。
            let _ = http::write_response_keep_alive(rd.get_mut(), 200, "OK", "text/html; charset=utf-8", PHONE_PAGE.as_bytes());
            w.note_page_load();
            continue;
        }

        // 证书下载：手机安装并信任后，WSS 不再被拦。
        if path == "/cert" {
            let der = w.cert.lock().map(|c| c.clone()).unwrap_or_default();
            if der.is_empty() {
                let _ = http::write_status(rd.get_mut(), 404, "Not Found");
                return;
            }
            let headers: Vec<(&str, String)> = vec![
                ("Content-Type", "application/x-x509-ca-cert".to_string()),
                ("Content-Length", der.len().to_string()),
                ("Content-Disposition", "attachment; filename=\"monitor.crt\"".to_string()),
                ("Cache-Control", "no-store".to_string()),
                ("Connection", "close".to_string()),
            ];
            let _ = http::write_head(rd.get_mut(), 200, "OK", &headers);
            let _ = rd.get_mut().write_all(&der);
            let _ = rd.get_mut().flush();
            return;
        }

        if path == "/favicon.ico" {
            let _ = http::write_status(rd.get_mut(), 204, "No Content");
            return;
        }
        let _ = http::write_status(rd.get_mut(), 404, "Not Found");
        return;
    }
}

fn ws_loop(w: Arc<Wireless>, mut ws: WsConn<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>) {
    let mut authed = false;
    let mut name: Option<String> = None;
    loop {
        let msg = match ws.read_message() {
            Ok(Some(m)) => m,
            _ => break,
        };
        match msg {
            WsMsg::Binary(data) => {
                if authed {
                    let len = data.len();
                    w.push_frame(data);
                    w.note_frame(len, true);
                }
            }
            WsMsg::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("hello") => {
                        let token = v.get("token").and_then(|t| t.as_str()).unwrap_or_default();
                        if token.is_empty() || token != w.token() {
                            w.note_error("配对码无效：手机 WSS 握手被拒");
                            let _ = ws.write_text("{\"type\":\"error\",\"message\":\"配对码无效\"}");
                            break;
                        }
                        authed = true;
                        name = v.get("name").and_then(|t| t.as_str()).map(|s| s.trim().to_string());
                        if let Ok(mut c) = w.connected.lock() {
                            *c = name.clone();
                        }
                        let _ = w.tx.send(Event::Wireless { connected: true, name: name.clone(), hint: None });
                        let _ = ws.write_text("{\"type\":\"hello-ok\"}");
                    }
                    Some("name") => {
                        let new_name = v.get("name").and_then(|t| t.as_str()).map(|s| s.trim().to_string());
                        if authed && new_name != name {
                            name = new_name;
                            if let Ok(mut c) = w.connected.lock() {
                                *c = name.clone();
                            }
                            let _ = w.tx.send(Event::Wireless { connected: true, name: name.clone(), hint: None });
                        }
                    }
                    Some("ping") => {
                        let _ = ws.write_text("{\"type\":\"pong\"}");
                    }
                    _ => {}
                }
            }
            WsMsg::Ping(data) => {
                let _ = ws.write_pong(&data);
            }
            WsMsg::Pong => {}
            WsMsg::Close => break,
        }
    }
    let _ = ws.close();
    if authed {
        if let Ok(mut c) = w.connected.lock() {
            *c = None;
        }
        let _ = w.tx.send(Event::Wireless { connected: false, name, hint: Some("手机已断开连接".into()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn token_is_hex_and_not_empty() {
        let t = random_token();
        assert!(t.len() >= 8);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_ipconfig_addresses() {
        let text = "以太网适配器 以太网:

   连接特定的 DNS 后缀 . . . . . . . :
   IPv4 地址 . . . . . . . . . . . . : 192.168.31.7
   子网掩码  . . . . . . . . . . . . : 255.255.255.0

无线局域网适配器 WLAN:
   IPv4 Address. . . . . . . . . . . : 10.0.0.23(Preferred)
   IPv4 地址 . . . . . . . . . . . . : 127.0.0.1";
        let ips = lan_ips_from_ipconfig(text);
        assert_eq!(ips, vec!["192.168.31.7".to_string(), "10.0.0.23".to_string()]);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn lan_ips_prefer_wifi_and_skip_vpn() {
        let sample = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\nen0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tinet 192.168.31.24 netmask 0xffffff00 broadcast 192.168.31.255\nen1: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tinet 10.0.0.7 netmask 0xffff0000 broadcast 10.0.255.255\nutun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1380\n\tinet 10.8.0.2 --> 10.8.0.2 netmask 0xffffffff\n";
        assert_eq!(lan_ips_from(sample), vec!["192.168.31.24".to_string(), "10.0.0.7".to_string()]);
    }

    #[test]
    fn refresh_updates_url_and_keeps_port_token() {
        let (tx, _rx) = channel();
        let w = Wireless::new(tx);
        let first = w.ensure_started();
        if !first.available {
            w.stop();
            return; // 无局域网环境的机器跳过
        }
        let refreshed = w.refresh(None);
        assert!(refreshed.available);
        assert_eq!(first.port, refreshed.port);
        assert_eq!(first.token, refreshed.token);
        assert!(!refreshed.ips.is_empty());
        assert!(refreshed.url.starts_with(&format!("https://{}:{}/", refreshed.lan, refreshed.port)));
        assert_eq!(&refreshed.qr[..8], b"\x89PNG\r\n\x1a\n");
        w.stop();
    }

    #[test]
    fn rejects_non_lan_addresses() {
        assert!(is_lan_v4("192.168.1.5"));
        assert!(!is_lan_v4("127.0.0.1"));
        assert!(!is_lan_v4("169.254.10.2"));
        assert!(!is_lan_v4("255.255.255.255"));
        assert!(!is_lan_v4("224.0.0.251"));
        assert!(!is_lan_v4("not-an-ip"));
    }

    #[test]
    fn qr_png_has_png_signature() {
        let png = make_qr_png("https://192.168.1.8:51234/?t=abcdef0123456789").expect("qr");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    fn client_tls() -> Arc<rustls::ClientConfig> {
        #[derive(Debug)]
        struct Accept(rustls::crypto::CryptoProvider);
        impl rustls::client::danger::ServerCertVerifier for Accept {
            fn verify_server_cert(
                &self,
                _end_entity: &rustls::pki_types::CertificateDer<'_>,
                _intermediates: &[rustls::pki_types::CertificateDer<'_>],
                _server_name: &rustls::pki_types::ServerName<'_>,
                _ocsp: &[u8],
                _now: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
            }
            fn verify_tls13_signature(
                &self,
                message: &[u8],
                cert: &rustls::pki_types::CertificateDer<'_>,
                dss: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
                rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                self.0.signature_verification_algorithms.supported_schemes()
            }
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = rustls::crypto::ring::default_provider();
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider.clone()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Accept(provider)))
            .with_no_client_auth();
        Arc::new(cfg)
    }

    fn connect(port: u16) -> std::io::BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap().to_owned();
        let conn = rustls::ClientConnection::new(client_tls(), name).unwrap();
        std::io::BufReader::new(rustls::StreamOwned::new(conn, sock))
    }

    fn read_head(rd: &mut impl std::io::BufRead) -> String {
        let mut out = String::new();
        loop {
            let mut line = String::new();
            if rd.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            out.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        out
    }

    fn read_body(rd: &mut impl std::io::BufRead, head: &str) -> Vec<u8> {
        let mut len = 0usize;
        for line in head.lines() {
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        if len > 0 {
            let _ = rd.read_exact(&mut body);
        }
        body
    }

    /// 端到端：手机页可访问、token 校验生效、WS 握手后 JPEG 帧能送达 sink。
    #[test]
    fn serves_phone_page_and_forwards_ws_frames() {
        let (tx, events) = channel();
        let w = Wireless::new(tx);
        let info = w.ensure_started();
        if !info.available {
            eprintln!("跳过：无线服务不可用（{}）", info.hint);
            return;
        }
        let (frames, pending) = w.subscribe();
        assert!(pending.is_none(), "还没有帧时不应有缓冲帧");

        // 1. 手机页：正确 token 返回 HTML，错误 token 返回 403
        let mut rd = connect(info.port);
        let req = format!("GET /?t={} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", info.token, info.lan);
        rd.get_mut().write_all(req.as_bytes()).unwrap();
        let head = read_head(&mut rd);
        assert!(head.starts_with("HTTP/1.1 200"), "手机页应返回 200：{head}");
        assert!(head.contains("text/html"), "手机页应为 HTML：{head}");
        let body = read_body(&mut rd, &head);
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("getUserMedia"), "手机页应包含取摄像头逻辑");
        assert!(html.contains("/ws"), "手机页应连接 /ws");

        let mut rd = connect(info.port);
        let req = "GET /?t=bad-token HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        rd.get_mut().write_all(req.as_bytes()).unwrap();
        let head = read_head(&mut rd);
        assert!(head.starts_with("HTTP/1.1 403"), "错误 token 应被拒绝：{head}");

        // 2. WS：hello 鉴权 -> hello-ok -> 二进制帧进入 sink
        let mut rd = connect(info.port);
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            info.lan, key
        );
        rd.get_mut().write_all(req.as_bytes()).unwrap();
        let head = read_head(&mut rd);
        assert!(head.starts_with("HTTP/1.1 101"), "WS 应握手成功：{head}");
        let mut ws = WsConn::with_reader(rd);
        ws.write_text(&format!("{{\"type\":\"hello\",\"name\":\"测试手机\",\"token\":\"{}\"}}", info.token)).unwrap();
        match ws.read_message().unwrap() {
            Some(WsMsg::Text(t)) => assert!(t.contains("hello-ok"), "应收到 hello-ok：{t}"),
            other => panic!("期望 hello-ok，得到 {other:?}"),
        }
        let frame = vec![0xFF, 0xD8, 0x11, 0x22, 0xFF, 0xD9];
        ws.write_binary(&frame).unwrap();
        let got = frames.recv_timeout(Duration::from_secs(3)).expect("后端应收到手机帧");
        assert_eq!(got, frame);

        // 3. 事件流：手机上线通知
        let mut connected = false;
        for _ in 0..20 {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Wireless { connected: true, .. }) => {
                    connected = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(connected, "应收到手机上线事件");
        w.clear_sink();
    }

    /// 兜底通道：WSS 握手失败时手机改用 HTTPS POST 逐帧推流（同一连接复用）。
    #[test]
    fn accepts_post_frames_and_serves_certificate() {
        let (tx, events) = channel();
        let w = Wireless::new(tx);
        let info = w.ensure_started();
        if !info.available {
            eprintln!("跳过：无线服务不可用（{}）", info.hint);
            return;
        }
        let (frames, pending) = w.subscribe();
        assert!(pending.is_none());

        let mut rd = connect(info.port);
        let frame = vec![0xFF, 0xD8, 0x33, 0x44, 0xFF, 0xD9];
        let post = |rd: &mut std::io::BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>, body: &[u8]| {
            let head = format!(
                "POST /frame?t={}&name=%E6%B5%8B%E8%AF%95%E6%89%8B%E6%9C%BA HTTP/1.1\r\nHost: {}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                info.token, info.lan, body.len()
            );
            rd.get_mut().write_all(head.as_bytes()).unwrap();
            rd.get_mut().write_all(body).unwrap();
            read_head(rd)
        };
        let resp = post(&mut rd, &frame);
        assert!(resp.starts_with("HTTP/1.1 204"), "POST /frame 应返回 204：{resp}");
        assert_eq!(frames.recv_timeout(Duration::from_secs(3)).expect("应收到 POST 帧"), frame);

        // 同一条连接再推一帧：keep-alive 生效
        let frame2 = vec![0xFF, 0xD8, 0x55, 0x66, 0xFF, 0xD9];
        let resp2 = post(&mut rd, &frame2);
        assert!(resp2.starts_with("HTTP/1.1 204"), "复用连接的第二帧应被接受：{resp2}");
        assert_eq!(frames.recv_timeout(Duration::from_secs(3)).expect("应收到第二帧"), frame2);

        // 诊断计数：界面靠它区分「没连上」和「连上了没推流」
        let d = w.diag();
        assert_eq!(d["framePosts"], 2, "应记到两帧：{d}");
        assert_eq!(d["connected"], true, "应记为已连接：{d}");
        assert_eq!(d["hasFrame"], true, "应记录收到过帧：{d}");
        assert_eq!(d["lastRequest"], "POST /frame", "应记到最近一次请求：{d}");

        // 错误 token 被拒绝
        let mut bad = connect(info.port);
        bad.get_mut()
            .write_all(b"POST /frame?t=nope HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n\xff\xd8\x00\x00")
            .unwrap();
        assert!(read_head(&mut bad).starts_with("HTTP/1.1 403"), "错误 token 的 POST 应被拒绝");

        // 第一帧到达即触发上线事件
        let mut connected = false;
        for _ in 0..20 {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Wireless { connected: true, .. }) => {
                    connected = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(connected, "POST 帧应触发手机上线事件");

        // 证书下载
        let mut rd = connect(info.port);
        let req = format!("GET /cert HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", info.lan);
        rd.get_mut().write_all(req.as_bytes()).unwrap();
        let head = read_head(&mut rd);
        assert!(head.starts_with("HTTP/1.1 200"), "证书应可下载：{head}");
        assert!(head.to_ascii_lowercase().contains("application/x-x509-ca-cert"), "证书类型不对：{head}");
        let der = read_body(&mut rd, &head);
        assert_eq!(&der[..1], &[0x30], "DER 证书应以 SEQUENCE 开头");
        w.clear_sink();
    }

    /// 手机先连上、桌面后开源：缓冲的最新一帧要立刻交给新订阅者。
    #[test]
    fn keeps_latest_frame_for_late_subscribers() {
        let (tx, _rx) = channel();
        let w = Wireless::new(tx);
        w.push_frame(vec![0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        w.push_frame(vec![0xFF, 0xD8, 0x03, 0x04, 0xFF, 0xD9]);
        let (_frames, pending) = w.subscribe();
        assert_eq!(pending.expect("应缓冲最新一帧"), vec![0xFF, 0xD8, 0x03, 0x04, 0xFF, 0xD9]);
        let (_second, none) = w.subscribe();
        assert!(none.is_none(), "缓冲帧只交付一次");
    }

    /// 持续推流：一条 keep-alive 连接连推 200 帧，全部要被接受且顺序不乱。
    #[test]
    fn keeps_post_channel_healthy_under_sustained_load() {
        let (tx, _events) = channel();
        let w = Wireless::new(tx);
        let info = w.ensure_started();
        if !info.available {
            eprintln!("跳过：无线服务不可用（{}）", info.hint);
            return;
        }
        let (frames, _pending) = w.subscribe();
        let mut rd = connect(info.port);
        let total = 200usize;
        for i in 0..total {
            let body = vec![0xFF, 0xD8, i as u8, (i >> 8) as u8, 0x11, 0x22, 0xFF, 0xD9];
            let head = format!(
                "POST /frame?t={}&name=stress HTTP/1.1\r\nHost: {}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                info.token, info.lan, body.len()
            );
            rd.get_mut().write_all(head.as_bytes()).unwrap();
            rd.get_mut().write_all(&body).unwrap();
            let resp = read_head(&mut rd);
            assert!(resp.starts_with("HTTP/1.1 204"), "第 {i} 帧被拒：{resp}");
            let got = frames.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| panic!("第 {i} 帧没到 sink"));
            assert_eq!(got, body, "第 {i} 帧内容不一致");
        }
        w.clear_sink();
    }
}

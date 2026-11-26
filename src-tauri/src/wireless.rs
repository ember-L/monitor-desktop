//! 手机无线推流：局域网 HTTPS 提供手机页，WSS 接收手机摄像头 JPEG 帧。
//! 自签证书（rcgen + rustls ring），端口随机，配对靠 URL 里的随机 token。

use crate::http::{self, WsConn, WsMsg};
use crate::source::Event;
use serde::Serialize;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PHONE_PAGE: &str = include_str!("phone_page.html");

/// 配对信息（前端展示二维码与地址）。
#[derive(Clone, Serialize)]
pub struct PairInfo {
    pub url: String,
    pub lan: String,
    pub port: u16,
    pub token: String,
    pub available: bool,
    pub hint: String,
    #[serde(skip)]
    pub qr: Vec<u8>,
}

pub struct Wireless {
    tx: Sender<Event>,
    sink: Mutex<Option<Sender<Vec<u8>>>>,
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
            info: Mutex::new(None),
            connected: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            starting: Mutex::new(false),
        })
    }

    fn unavailable(hint: String) -> PairInfo {
        PairInfo { url: String::new(), lan: String::new(), port: 0, token: String::new(), available: false, hint, qr: Vec::new() }
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
        let Some(lan) = lan_ip() else {
            return Self::unavailable("未连接局域网，请接入 Wi-Fi 或有线网络后重试".into());
        };
        let token = random_token();
        let config = match tls_config(&lan) {
            Ok(c) => c,
            Err(e) => return Self::unavailable(format!("生成自签证书失败：{e}")),
        };
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

    pub fn info(&self) -> Option<PairInfo> {
        self.info.lock().ok().and_then(|i| i.clone())
    }

    pub fn subscribe(&self) -> Receiver<Vec<u8>> {
        let (tx, rx) = channel();
        if let Ok(mut sink) = self.sink.lock() {
            *sink = Some(tx);
        }
        rx
    }

    pub fn clear_sink(&self) {
        if let Ok(mut sink) = self.sink.lock() {
            *sink = None;
        }
    }

    fn push_frame(&self, data: Vec<u8>) {
        if let Ok(sink) = self.sink.lock() {
            if let Some(tx) = sink.as_ref() {
                let _ = tx.send(data);
            }
        }
    }

    fn token(&self) -> String {
        self.info.lock().ok().and_then(|i| i.as_ref().map(|p| p.token.clone())).unwrap_or_default()
    }
}

fn tls_config(lan: &str) -> Result<Arc<rustls::ServerConfig>, String> {
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
    Ok(Arc::new(config))
}

/// 通过 UDP connect 探测出口网卡地址（不会真的发包）。
fn lan_ip() -> Option<String> {
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
    use std::io::Read;
    let mut bytes = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok() {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{n:x}")
    }
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

fn serve_conn(w: Arc<Wireless>, cfg: Arc<rustls::ServerConfig>, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
    let tls = rustls::StreamOwned::new(conn, stream);
    let mut rd = BufReader::new(tls);
    let req = match http::read_request(&mut rd) {
        Ok(r) => r,
        Err(_) => return,
    };
    let path = req.path.clone();

    if path == "/ws" {
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

    // 手机页与图标：需要正确配对 token
    if path == "/" || path == "/index.html" {
        let token_ok = req.query_param("t").as_deref() == Some(w.token().as_str());
        if !token_ok {
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
        let _ = http::write_response(rd.get_mut(), 200, "OK", "text/html; charset=utf-8", &[], PHONE_PAGE.as_bytes());
        return;
    }
    if path == "/favicon.ico" {
        let _ = http::write_status(rd.get_mut(), 204, "No Content");
        return;
    }
    let _ = http::write_status(rd.get_mut(), 404, "Not Found");
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
                    w.push_frame(data);
                }
            }
            WsMsg::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("hello") => {
                        let token = v.get("token").and_then(|t| t.as_str()).unwrap_or_default();
                        if token.is_empty() || token != w.token() {
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
    use std::io::{Read, Write};

    #[test]
    fn token_is_hex_and_not_empty() {
        let t = random_token();
        assert!(t.len() >= 8);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token());
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
        let frames = w.subscribe();

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
}

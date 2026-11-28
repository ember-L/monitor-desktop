//! 本机取景器 HTTP 服务：给 WebView 推送 JPEG 预览，并提供素材文件/缩略图/配对二维码。
//! 只监听 127.0.0.1 随机端口，进程退出即失效。

use crate::capture;
use crate::http::{self, parse_range, WsConn, WsMsg};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine as _};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Latest {
    seq: u64,
    jpeg: Option<Arc<Vec<u8>>>,
}

pub struct Viewer {
    pub port: u16,
    root: PathBuf,
    preview_token: String,
    latest: Mutex<Latest>,
    cv: Condvar,
    qr: Mutex<Option<Arc<Vec<u8>>>>,
    thumbs: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl Viewer {
    pub fn start(root: PathBuf) -> std::io::Result<Arc<Viewer>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let mut secret = [0; 16];
        rustls::crypto::ring::default_provider().secure_random.fill(&mut secret)
            .map_err(|_| std::io::Error::other("无法生成本机预览令牌"))?;
        let viewer = Arc::new(Viewer {
            port,
            root,
            preview_token: URL_SAFE_NO_PAD.encode(secret),
            latest: Mutex::new(Latest { seq: 0, jpeg: None }),
            cv: Condvar::new(),
            qr: Mutex::new(None),
            thumbs: Mutex::new(HashMap::new()),
        });
        let me = viewer.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let v = me.clone();
                std::thread::spawn(move || v.handle(stream));
            }
        });
        Ok(viewer)
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base(), path)
    }

    pub fn preview_url(&self) -> String {
        format!("ws://127.0.0.1:{}/preview?t={}", self.port, self.preview_token)
    }

    pub fn set_frame(&self, jpeg: Arc<Vec<u8>>) {
        if let Ok(mut l) = self.latest.lock() {
            l.seq = l.seq.wrapping_add(1);
            l.jpeg = Some(jpeg);
        }
        self.cv.notify_all();
    }

    pub fn clear(&self) {
        if let Ok(mut l) = self.latest.lock() {
            l.seq = l.seq.wrapping_add(1);
            l.jpeg = None;
        }
        self.cv.notify_all();
    }

    /// 取一帧；指定 since 时最多等 wait，直到出现帧号不同的新帧。
    /// 前端用它做长轮询：请求发出后一有新帧立刻返回，比固定间隔轮询少等一整个间隔。
    fn snapshot(&self, since: Option<u64>, wait: Duration) -> Option<(u64, Arc<Vec<u8>>)> {
        let deadline = Instant::now() + wait;
        let mut l = self.latest.lock().ok()?;
        loop {
            let fresh = match since {
                Some(s) => s != l.seq,
                None => true,
            };
            if fresh {
                if let Some(jpeg) = &l.jpeg {
                    return Some((l.seq, jpeg.clone()));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self.cv.wait_timeout(l, deadline - now).ok()?;
            l = guard;
        }
    }

    pub fn set_qr(&self, png: Vec<u8>) {
        if let Ok(mut q) = self.qr.lock() {
            *q = Some(Arc::new(png));
        }
    }

    pub fn put_thumb(&self, name: &str, jpeg: Vec<u8>) {
        if let Ok(mut t) = self.thumbs.lock() {
            t.insert(name.to_string(), Arc::new(jpeg));
        }
    }

    pub fn forget_thumb(&self, name: &str) {
        if let Ok(mut t) = self.thumbs.lock() {
            t.remove(name);
        }
    }

    fn handle(&self, mut stream: TcpStream) {
        // 本机回环也要关 Nagle：逐帧小响应攒包会让每帧多等几十毫秒。
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let mut rd = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let req = match http::read_request(&mut rd) {
            Ok(r) => r,
            Err(_) => return,
        };
        let path = req.path.clone();
        if path == "/preview" {
            let _ = self.serve_preview(&req, rd);
            return;
        }
        if path == "/stream" {
            self.serve_stream(stream);
            return;
        }
        // 单帧快照：给不支持长连接流式读取的 WebView 兜底轮询用。
        if path == "/frame.jpg" || path == "/frame" {
            // v= 上一次拿到的帧号，wait= 最多等多久（毫秒）。
            // 带 v 的请求会挂起到有新帧才返回：前端不用固定 30ms 空转一次，
            // 既省掉平均一个轮询间隔的延迟，也少掉大量空请求。
            let since = req.query_param("v").and_then(|v| v.parse::<u64>().ok());
            let wait = req
                .query_param("wait")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                .min(2_000);
            let got = self.snapshot(since, Duration::from_millis(wait));
            let mut extra = vec![("Access-Control-Allow-Origin", "*".to_string())];
            extra.push(("Access-Control-Expose-Headers", "X-Frame-Seq".to_string()));
            match got {
                Some((seq, bytes)) => {
                    extra.push(("X-Frame-Seq", seq.to_string()));
                    let _ = http::write_response(&mut stream, 200, "OK", "image/jpeg", &extra, &bytes);
                }
                None if since.is_some() => {
                    // 等到超时也没有新帧：204 让前端立刻续上下一次等待，不算错误。
                    let _ = http::write_response(&mut stream, 204, "No Content", "text/plain", &extra, b"");
                }
                None => {
                    // 404 也要带 CORS 头：否则 WKWebView 里 fetch 会直接抛 TypeError，
                    // 前端分不清「还没收到帧」和「取景服务挂了」。
                    let _ = http::write_response(&mut stream, 404, "Not Found", "text/plain", &extra, b"");
                }
            }
            return;
        }
        if path == "/qr" {
            let png = self.qr.lock().ok().and_then(|q| q.clone());
            match png {
                Some(bytes) => {
                    let extra = [("Access-Control-Allow-Origin", "*".to_string())];
                    let _ = http::write_response(&mut stream, 200, "OK", "image/png", &extra, &bytes);
                }
                None => {
                    let _ = http::write_status(&mut stream, 404, "Not Found");
                }
            }
            return;
        }
        if path == "/health" {
            let extra = [("Access-Control-Allow-Origin", "*".to_string())];
            let _ = http::write_response(&mut stream, 200, "OK", "text/plain", &extra, b"ok");
            return;
        }
        if let Some(name) = path.strip_prefix("/media/") {
            self.serve_media(stream, &req.headers, name, false);
            return;
        }
        if let Some(name) = path.strip_prefix("/thumb/") {
            self.serve_media(stream, &req.headers, name, true);
            return;
        }
        let _ = http::write_status(&mut stream, 404, "Not Found");
    }

    fn safe_name(name: &str) -> Option<String> {
        let decoded = name.to_string();
        if decoded.is_empty() || decoded.contains('/') || decoded.contains('\\') || decoded.contains("..") {
            return None;
        }
        Some(decoded)
    }

    fn serve_media(&self, mut stream: TcpStream, headers: &[(String, String)], raw_name: &str, thumb: bool) {
        let Some(name) = Self::safe_name(raw_name) else {
            let _ = http::write_status(&mut stream, 400, "Bad Request");
            return;
        };
        let path = self.root.join(&name);
        if !path.is_file() {
            let _ = http::write_status(&mut stream, 404, "Not Found");
            return;
        }
        if thumb {
            if let Ok(cache) = self.thumbs.lock() {
                if let Some(bytes) = cache.get(&name) {
                    let _ = http::write_response(&mut stream, 200, "OK", "image/jpeg", &[], bytes);
                    return;
                }
            }
            let generated = if name.to_ascii_lowercase().ends_with(".mp4") {
                capture::video_thumb(&path)
            } else {
                std::fs::read(&path).ok().and_then(|b| capture::make_thumb(&b, 200))
            };
            match generated {
                Some(t) => {
                    self.put_thumb(&name, t.jpeg.clone());
                    let _ = http::write_response(&mut stream, 200, "OK", "image/jpeg", &[], &t.jpeg);
                }
                None => {
                    let _ = http::write_status(&mut stream, 404, "Not Found");
                }
            }
            return;
        }
        let is_video = name.to_ascii_lowercase().ends_with(".mp4");
        let content_type = if is_video { "video/mp4" } else { "image/jpeg" };
        let Ok(meta) = std::fs::metadata(&path) else {
            let _ = http::write_status(&mut stream, 404, "Not Found");
            return;
        };
        let len = meta.len();
        let range = headers
            .iter()
            .find(|(k, _)| k == "range")
            .and_then(|(_, v)| parse_range(v, len));
        match range {
            Some(r) => {
                let count = r.end - r.start + 1;
                let mut file = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = http::write_status(&mut stream, 500, "Internal Server Error");
                        return;
                    }
                };
                use std::io::{Seek, SeekFrom};
                let _ = file.seek(SeekFrom::Start(r.start));
                let mut body = vec![0u8; count as usize];
                use std::io::Read;
                let mut got = 0usize;
                while got < body.len() {
                    match file.read(&mut body[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(_) => break,
                    }
                }
                body.truncate(got);
                let _ = http::write_head(
                    &mut stream,
                    206,
                    "Partial Content",
                    &[
                        ("Content-Type", content_type.to_string()),
                        ("Content-Length", body.len().to_string()),
                        ("Content-Range", format!("bytes {}-{}/{}", r.start, r.end, len)),
                        ("Accept-Ranges", "bytes".to_string()),
                        ("Cache-Control", "no-store".to_string()),
                        ("Connection", "close".to_string()),
                    ],
                );
                let _ = stream.write_all(&body);
            }
            None => {
                let body = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => {
                        let _ = http::write_status(&mut stream, 500, "Internal Server Error");
                        return;
                    }
                };
                let _ = http::write_head(
                    &mut stream,
                    200,
                    "OK",
                    &[
                        ("Content-Type", content_type.to_string()),
                        ("Content-Length", body.len().to_string()),
                        ("Accept-Ranges", "bytes".to_string()),
                        ("Cache-Control", "no-store".to_string()),
                        ("Connection", "close".to_string()),
                    ],
                );
                let _ = stream.write_all(&body);
            }
        }
    }

    /// MJPEG：每帧一个 JPEG part，紧跟最新帧；客户端断开即退出。
    fn serve_preview(&self, req: &http::Request, mut rd: BufReader<TcpStream>) -> std::io::Result<()> {
        // WKWebView 可能把自定义协议的 Origin 序列化为 null；令牌仅通过 Tauri IPC 下发。
        if req.query_param("t").as_deref() != Some(self.preview_token.as_str())
            || !matches!(req.header("origin"), Some("tauri://localhost" | "http://tauri.localhost" | "null"))
        {
            return http::write_status(rd.get_mut(), 403, "Forbidden");
        }
        let Some(key) = req.header("sec-websocket-key").filter(|key| STANDARD.decode(key).is_ok_and(|bytes| bytes.len() == 16)) else {
            return http::write_status(rd.get_mut(), 400, "Bad Request");
        };
        if req.method != "GET" || req.version != "HTTP/1.1" || !req.is_websocket_upgrade()
            || req.header("sec-websocket-version") != Some("13")
            || !req.header("connection").is_some_and(|v| v.split(',').any(|part| part.trim().eq_ignore_ascii_case("upgrade")))
        {
            return http::write_status(rd.get_mut(), 400, "Bad Request");
        }
        rd.get_ref().set_write_timeout(Some(Duration::from_secs(3)))?;
        http::write_head(rd.get_mut(), 101, "Switching Protocols", &[
            ("Upgrade", "websocket".into()),
            ("Connection", "Upgrade".into()),
            ("Sec-WebSocket-Accept", http::ws_accept_key(key)),
        ])?;
        let mut ws = WsConn::with_reader(rd);
        let mut seq = None;
        while let Some(msg) = ws.read_message()? {
            match msg {
                WsMsg::Text(text) if text == "next" => {
                    // 浏览器上一帧处理完才索取下一帧；慢时跳到最新帧，不累积延迟。
                    if let Some((current, jpeg)) = self.snapshot(seq, Duration::from_secs(1)) {
                        ws.write_binary(&jpeg)?;
                        seq = Some(current);
                    } else {
                        ws.write_text("idle")?;
                    }
                }
                WsMsg::Ping(data) => ws.write_pong(&data)?,
                WsMsg::Pong => {},
                _ => break,
            }
        }
        ws.close()
    }

    fn serve_stream(&self, mut stream: TcpStream) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nPragma: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        #[cfg(debug_assertions)]
        eprintln!("[monitor] 预览流已连接（MJPEG 客户端）");
        let mut seen = 0u64;
        loop {
            let (jpeg, seq) = {
                let Ok(mut l) = self.latest.lock() else { return };
                while l.seq == seen {
                    match self.cv.wait_timeout(l, Duration::from_millis(500)) {
                        Ok((guard, _)) => l = guard,
                        Err(_) => return,
                    }
                }
                (l.jpeg.clone(), l.seq)
            };
            seen = seq;
            let Some(jpeg) = jpeg else { continue };
            let header = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            );
            if stream.write_all(header.as_bytes()).is_err() || stream.write_all(&jpeg).is_err() {
                return;
            }
            let _ = stream.write_all(b"\r\n");
            let _ = stream.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("monitor-viewer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn connect(port: u16) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    fn request(port: u16, raw: &str) -> Vec<u8> {
        let mut s = connect(port);
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    fn preview_request(target: &str, origin: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: x\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
    }

    #[test]
    fn preview_requires_token_and_application_origin() {
        let root = temp_root("preview-forbidden");
        let viewer = Viewer::start(root.clone()).unwrap();
        let target = format!("/preview?t={}", viewer.preview_token);
        for (path, origin) in [("/preview", "null"), (&target, "https://example.org")] {
            let raw = request(viewer.port, &preview_request(path, origin));
            assert!(header_of(&raw).starts_with("HTTP/1.1 403"), "{}", header_of(&raw));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preview_sends_latest_only_and_heartbeats_when_idle() {
        let root = temp_root("preview-ws");
        let viewer = Viewer::start(root.clone()).unwrap();
        let target = format!("/preview?t={}", viewer.preview_token);
        let mut rd = BufReader::new(connect(viewer.port));
        rd.get_mut().write_all(preview_request(&target, "http://tauri.localhost").as_bytes()).unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 101"), "{line}");
        while line != "\r\n" {
            line.clear();
            assert!(rd.read_line(&mut line).unwrap() > 0);
        }
        let mut ws = WsConn::with_reader(rd);
        let jpeg = Arc::new(vec![1; 540_000]);
        viewer.set_frame(jpeg.clone());
        ws.write_text("next").unwrap();
        assert!(matches!(ws.read_message().unwrap(), Some(WsMsg::Binary(data)) if data == *jpeg));
        viewer.set_frame(Arc::new(vec![2; jpeg.len()]));
        let newest = Arc::new(vec![3; jpeg.len()]);
        viewer.set_frame(newest.clone());
        ws.write_text("next").unwrap();
        assert!(matches!(ws.read_message().unwrap(), Some(WsMsg::Binary(data)) if data == *newest));
        ws.write_text("next").unwrap();
        assert!(matches!(ws.read_message().unwrap(), Some(WsMsg::Text(text)) if text == "idle"));
        ws.close().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn serves_health_plaintext() {
        let root = temp_root("health");
        let viewer = Viewer::start(root.clone()).unwrap();
        let body = request(viewer.port, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("ok"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn streams_latest_jpeg_as_multipart() {
        let root = temp_root("stream");
        let viewer = Viewer::start(root.clone()).unwrap();
        let jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0xFF, 0xD9];
        viewer.set_frame(Arc::new(jpeg.clone()));

        let mut s = connect(viewer.port);
        s.write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut acc = Vec::new();
        let mut buf = [0u8; 512];
        while acc.len() < 256 {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&acc).to_string();
        assert!(text.contains("multipart/x-mixed-replace; boundary=frame"), "{text}");
        assert!(text.contains("--frame"), "{text}");
        assert!(text.contains(&format!("Content-Length: {}", jpeg.len())), "{text}");
        assert!(acc.windows(jpeg.len()).any(|w| w == jpeg.as_slice()), "payload bytes missing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_frame_returns_cors_aware_404() {
        let root = temp_root("noframe");
        let viewer = Viewer::start(root.clone()).unwrap();
        let raw = request(
            viewer.port,
            "GET /frame.jpg HTTP/1.1\r\nHost: x\r\nOrigin: tauri://localhost\r\n\r\n",
        );
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(text.starts_with("http/1.1 404 not found"), "{text}");
        assert!(text.contains("access-control-allow-origin: *"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn serves_media_with_and_without_range() {
        let root = temp_root("media");
        let payload: Vec<u8> = (0u8..16).collect();
        std::fs::write(root.join("photo.jpg"), &payload).unwrap();
        let viewer = Viewer::start(root.clone()).unwrap();

        let full = request(viewer.port, "GET /media/photo.jpg HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(String::from_utf8_lossy(&full).starts_with("HTTP/1.1 200 OK"));
        assert!(full.windows(payload.len()).any(|w| w == payload.as_slice()));

        let partial = request(
            viewer.port,
            "GET /media/photo.jpg HTTP/1.1\r\nHost: x\r\nRange: bytes=4-7\r\n\r\n",
        );
        let head = String::from_utf8_lossy(&partial).to_string();
        assert!(head.starts_with("HTTP/1.1 206 Partial Content"), "{head}");
        assert!(head.contains("Content-Range: bytes 4-7/16"), "{head}");
        assert!(partial.windows(4).any(|w| w == &payload[4..8]));

        let missing = request(viewer.port, "GET /media/../secret HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(String::from_utf8_lossy(&missing).starts_with("HTTP/1.1 400 Bad Request"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn header_of(raw: &[u8]) -> String {
        let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..head_end]).to_string()
    }

    #[test]
    fn frame_long_poll_times_out_with_204() {
        let root = temp_root("longpoll-empty");
        let viewer = Viewer::start(root.clone()).unwrap();
        let raw = request(viewer.port, "GET /frame.jpg?v=0&wait=120 HTTP/1.1\r\nHost: x\r\n\r\n");
        let head = header_of(&raw);
        assert!(head.starts_with("HTTP/1.1 204"), "{head}");
        assert!(head.to_ascii_lowercase().contains("access-control-allow-origin: *"), "{head}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frame_long_poll_wakes_on_next_frame() {
        let root = temp_root("longpoll-wake");
        let viewer = Viewer::start(root.clone()).unwrap();
        let first: Vec<u8> = vec![0xFF, 0xD8, 1, 2, 0xFF, 0xD9];
        let second: Vec<u8> = vec![0xFF, 0xD8, 3, 4, 5, 0xFF, 0xD9];
        viewer.set_frame(Arc::new(first));
        let port = viewer.port;
        let later = Arc::new(viewer);
        let pusher = Arc::clone(&later);
        let next = second.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            pusher.set_frame(Arc::new(next));
        });
        let start = Instant::now();
        let raw = request(port, "GET /frame.jpg?v=1&wait=2000 HTTP/1.1\r\nHost: x\r\n\r\n");
        let elapsed = start.elapsed();
        h.join().unwrap();
        let head = header_of(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("X-Frame-Seq: 2"), "{head}");
        assert!(raw.windows(second.len()).any(|w| w == second.as_slice()), "payload missing");
        assert!(elapsed < Duration::from_millis(1500), "waited {elapsed:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frame_without_version_returns_latest_immediately() {
        let root = temp_root("longpoll-latest");
        let viewer = Viewer::start(root.clone()).unwrap();
        let jpeg: Vec<u8> = vec![0xFF, 0xD8, 9, 0xFF, 0xD9];
        viewer.set_frame(Arc::new(jpeg.clone()));
        let start = Instant::now();
        let raw = request(viewer.port, "GET /frame.jpg?wait=2000 HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(start.elapsed() < Duration::from_millis(500), "should not block");
        let head = header_of(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(raw.windows(jpeg.len()).any(|w| w == jpeg.as_slice()), "payload missing");
        let _ = std::fs::remove_dir_all(&root);
    }
}




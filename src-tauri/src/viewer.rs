//! 本机取景器 HTTP 服务：给 WebView 推送 MJPEG 预览，并提供素材文件/缩略图/配对二维码。
//! 只监听 127.0.0.1 随机端口，进程退出即失效。

use crate::capture;
use crate::http::{self, parse_range};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

struct Latest {
    seq: u64,
    jpeg: Option<Arc<Vec<u8>>>,
}

pub struct Viewer {
    pub port: u16,
    root: PathBuf,
    latest: Mutex<Latest>,
    cv: Condvar,
    qr: Mutex<Option<Arc<Vec<u8>>>>,
    thumbs: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl Viewer {
    pub fn start(root: PathBuf) -> std::io::Result<Arc<Viewer>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let viewer = Arc::new(Viewer {
            port,
            root,
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
        if path == "/stream" {
            self.serve_stream(stream);
            return;
        }
        // 单帧快照：给不支持长连接流式读取的 WebView 兜底轮询用。
        if path == "/frame.jpg" || path == "/frame" {
            let jpeg = self.latest.lock().ok().and_then(|l| l.jpeg.clone());
            match jpeg {
                Some(bytes) => {
                    let extra = [
                        ("Cache-Control", "no-store".to_string()),
                        ("Access-Control-Allow-Origin", "*".to_string()),
                    ];
                    let _ = http::write_response(&mut stream, 200, "OK", "image/jpeg", &extra, &bytes);
                }
                None => {
                    // 404 也要带 CORS 头：否则 WKWebView 里 fetch 会直接抛 TypeError，
                    // 前端分不清「还没收到帧」和「取景服务挂了」。
                    let extra = [("Access-Control-Allow-Origin", "*".to_string())];
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
    use std::io::{Read, Write};

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
}




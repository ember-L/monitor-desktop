//! 极简 HTTP/1.1 与 WebSocket 服务端工具：请求解析、响应写出、Range、WS 帧编解码。
//! 只服务于本机取景器（127.0.0.1）与局域网手机推流，不追求通用性。

use std::io::{self, BufRead, BufReader, Read, Write};

/// 解析结果保留完整请求行（method/version 目前未参与路由，便于以后扩展）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, key: &str) -> Option<String> {
        for pair in self.query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            if k == key {
                return Some(percent_decode(v));
            }
        }
        None
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

const MAX_HEAD: usize = 32 * 1024;

/// 读取请求头（不含 body）。请求行形如 `GET /path?q=1 HTTP/1.1`。
pub fn read_request<S: Read>(rd: &mut BufReader<S>) -> io::Result<Request> {
    let mut head = String::new();
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let n = rd.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "连接已关闭"));
        }
        total += n;
        if total > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头过大"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
    }
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok(Request { method, path, query, version, headers })
}

#[derive(Debug, PartialEq)]
pub struct ParsedRange {
    pub start: u64,
    pub end: u64,
}

/// 解析 `bytes=start-end`（仅单段）。总长为 `len` 时返回闭区间。
pub fn parse_range(header: &str, len: u64) -> Option<ParsedRange> {
    if len == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    let spec = spec.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let suffix: u64 = b.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (len.saturating_sub(suffix), len - 1)
    } else {
        let start: u64 = a.trim().parse().ok()?;
        let end = if b.trim().is_empty() { len - 1 } else { b.trim().parse().ok()? };
        (start, end.min(len - 1))
    };
    if start > end || start >= len {
        return None;
    }
    Some(ParsedRange { start, end })
}

pub fn write_head<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())
}

pub fn write_response<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", content_type.to_string()),
        ("Content-Length", body.len().to_string()),
        ("Cache-Control", "no-store".to_string()),
        ("Connection", "close".to_string()),
    ];
    headers.extend(extra.iter().cloned());
    write_head(w, status, reason, &headers)?;
    w.write_all(body)?;
    w.flush()
}

pub fn write_status<W: Write>(w: &mut W, status: u16, reason: &str) -> io::Result<()> {
    write_response(w, status, reason, "text/plain; charset=utf-8", &[], reason.as_bytes())
}

/// WebSocket 握手应答键：base64(sha1(key + GUID))。
pub fn ws_accept_key(key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

#[derive(Debug)]
pub enum WsMsg {
    Binary(Vec<u8>),
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// 服务端 WebSocket 连接：读客户端（必带掩码）帧，写服务端（不掩码）帧。
pub struct WsConn<S: Read + Write> {
    rd: BufReader<S>,
    closed: bool,
}

impl<S: Read + Write> WsConn<S> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(stream: S) -> Self {
        Self { rd: BufReader::with_capacity(64 * 1024, stream), closed: false }
    }

    /// 复用已解析过请求头的 BufReader（保留其中可能已缓冲的数据）。
    pub fn with_reader(rd: BufReader<S>) -> Self {
        Self { rd, closed: false }
    }

    fn read_exact_buf(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.rd.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn read_message(&mut self) -> io::Result<Option<WsMsg>> {
        let mut b0 = [0u8; 2];
        self.rd.read_exact(&mut b0)?;
        let opcode = b0[0] & 0x0F;
        let masked = b0[1] & 0x80 != 0;
        let len7 = (b0[1] & 0x7F) as u64;
        let len = match len7 {
            126 => {
                let b = self.read_exact_buf(2)?;
                u16::from_be_bytes([b[0], b[1]]) as u64
            }
            127 => {
                let b = self.read_exact_buf(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            }
            n => n,
        };
        if len > 16 * 1024 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "帧过大"));
        }
        let mask = if masked { self.read_exact_buf(4)? } else { Vec::new() };
        let mut payload = self.read_exact_buf(len as usize)?;
        if masked && mask.len() == 4 {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        Ok(Some(match opcode {
            0x1 => WsMsg::Text(String::from_utf8_lossy(&payload).to_string()),
            0x2 => WsMsg::Binary(payload),
            0x8 => WsMsg::Close,
            0x9 => WsMsg::Ping(payload),
            0xA => WsMsg::Pong,
            _ => WsMsg::Pong,
        }))
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let w = self.rd.get_mut();
        let mut head = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            head.push(len as u8);
        } else if len <= u16::MAX as usize {
            head.push(126);
            head.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            head.push(127);
            head.extend_from_slice(&(len as u64).to_be_bytes());
        }
        w.write_all(&head)?;
        w.write_all(payload)?;
        w.flush()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn write_binary(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_frame(0x2, data)
    }

    pub fn write_text(&mut self, s: &str) -> io::Result<()> {
        self.write_frame(0x1, s.as_bytes())
    }

    pub fn write_pong(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_frame(0xA, data)
    }

    pub fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self.write_frame(0x8, &[]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_request_line_query_and_headers() {
        let raw = b"GET /ws?t=abc%20d HTTP/1.1\r\nHost: x\r\nUpgrade: WebSocket\r\n\r\n";
        let mut rd = BufReader::new(Cursor::new(raw.to_vec()));
        let req = read_request(&mut rd).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/ws");
        assert_eq!(req.query_param("t").as_deref(), Some("abc d"));
        assert!(req.is_websocket_upgrade());
    }

    #[test]
    fn range_variants() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some(ParsedRange { start: 0, end: 99 }));
        assert_eq!(parse_range("bytes=500-", 1000), Some(ParsedRange { start: 500, end: 999 }));
        assert_eq!(parse_range("bytes=-100", 1000), Some(ParsedRange { start: 900, end: 999 }));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some(ParsedRange { start: 0, end: 999 }));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("chunks=0-1", 1000), None);
    }

    #[test]
    fn ws_accept_matches_rfc_vector() {
        assert_eq!(ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn reads_masked_binary_frame() {
        // 客户端帧：0x82, 0x80|len, mask(4), payload^mask
        let payload = b"hello";
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut raw = vec![0x82, 0x80 | payload.len() as u8];
        raw.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            raw.push(b ^ mask[i % 4]);
        }
        let mut conn = WsConn::new(Cursor::new(raw));
        match conn.read_message().unwrap().unwrap() {
            WsMsg::Binary(v) => assert_eq!(v, payload.to_vec()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn writes_unmasked_frame() {
        let mut conn = WsConn::new(Cursor::new(Vec::new()));
        conn.write_text("hi").unwrap();
        let out = conn.rd.into_inner().into_inner();
        assert_eq!(out[0], 0x81);
        assert_eq!(out[1], 2);
        assert_eq!(&out[2..], b"hi");
    }
}

#!/usr/bin/env python3
"""MONITOR 本地服务器

浏览器只允许「安全来源」（HTTPS 或 localhost）访问摄像头，所以手机通过
局域网访问时必须走 HTTPS。这个脚本会自动生成本地证书、同时启动 HTTPS
和 HTTP（跳转用）两个端口，并把局域网访问地址暴露给页面显示二维码。

用法:
    python3 serve.py                 # 默认 http://localhost:8080 + https://<LAN>:8443
    python3 serve.py --port 9000     # 自定义 HTTPS 端口
    python3 serve.py --no-tls        # 只用 HTTP（仅本机 localhost 可用摄像头）
    python3 serve.py --auth me:1234  # 局域网访问需要输入账号密码
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import threading
import time
import webbrowser
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).resolve().parent
CERT_DIR = ROOT / ".certs"
CERT_FILE = CERT_DIR / "cert.pem"
KEY_FILE = CERT_DIR / "key.pem"

CONFIG = {
    "https": False,
    "https_port": 8443,
    "http_port": 8080,
    "auth": None,
    "lan_ips": [],
    "hostname": socket.gethostname(),
}


def is_loopback(host: str) -> bool:
    """判断地址是否只对本机有效（这种地址手机打不开）。"""
    return host.strip("[]") in ("", "localhost", "127.0.0.1", "::1", "0.0.0.0")


def default_route_ip() -> str | None:
    """通过默认路由探测当前出网网卡的 IPv4（UDP connect 不会真的发包）。"""
    sock = None
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.connect(("8.8.8.8", 80))
        return sock.getsockname()[0]
    except OSError:
        return None
    finally:
        if sock is not None:
            sock.close()


def interface_ips() -> list[str]:
    """列出所有网卡上的 IPv4（macOS / Linux 通用，失败就返回空）。"""
    for cmd, pattern in (
        (["ifconfig"], r"inet (?:addr:)?(\d+\.\d+\.\d+\.\d+)"),
        (["ip", "-4", "-o", "addr"], r"inet (\d+\.\d+\.\d+\.\d+)/"),
    ):
        try:
            out = subprocess.run(cmd, capture_output=True, text=True, timeout=3).stdout
        except (OSError, subprocess.SubprocessError):
            continue
        found = re.findall(pattern, out)
        if found:
            return found
    return []


def lan_ips() -> list[str]:
    """实时获取本机当前的局域网 IPv4 地址，默认路由的网卡排在最前。

    每次调用都重新探测，所以换 Wi-Fi、DHCP 重新分配、插拔网线之后拿到的
    都是当下有效的地址，而不是服务启动时的旧快照。
    """
    found: list[str] = []

    def add(ip: str | None) -> None:
        if not ip or ip in found:
            return
        if ip.startswith("127.") or ip.startswith("169.254."):
            return  # 回环 / 链路本地（没连上网络时会出现）
        found.append(ip)

    add(default_route_ip())
    for ip in interface_ips():
        add(ip)
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            add(info[4][0])
    except OSError:
        pass
    return found


def ensure_cert() -> None:
    """生成本地自签名证书（含所有局域网 IP 的 SAN）。"""
    CERT_DIR.mkdir(exist_ok=True)
    ips = lan_ips()
    san = ["DNS:localhost", "DNS:" + socket.gethostname(), "IP:127.0.0.1"] + ["IP:" + ip for ip in ips]

    if CERT_FILE.exists() and KEY_FILE.exists():
        try:
            out = subprocess.run(
                ["openssl", "x509", "-in", str(CERT_FILE), "-noout", "-ext", "subjectAltName"],
                capture_output=True, text=True, timeout=10,
            ).stdout
            # openssl 输出的是 "IP Address:172.20.10.6" 这种写法，只比对 IP 本身
            have = set(re.findall(r"IP Address:([0-9.]+)", out))
            if "127.0.0.1" in have and all(ip in have for ip in ips):
                return
        except Exception:
            pass

    print("· 正在生成本地 HTTPS 证书…")
    subprocess.run(
        [
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(KEY_FILE), "-out", str(CERT_FILE),
            "-days", "825", "-subj", "/CN=MONITOR Local",
            "-addext", "subjectAltName=" + ",".join(san),
            "-addext", "basicConstraints=CA:FALSE",
            "-addext", "keyUsage=digitalSignature,keyEncipherment",
            "-addext", "extendedKeyUsage=serverAuth",
        ],
        check=True, capture_output=True,
    )
    os.chmod(KEY_FILE, 0o600)


_san_cache: dict = {"mtime": 0.0, "ips": []}


def cert_san_ips() -> list[str]:
    """读取当前证书里包含的 IP 列表（按 mtime 缓存，不会每次都跑 openssl）。"""
    if not CERT_FILE.exists():
        return []
    try:
        mtime = CERT_FILE.stat().st_mtime
    except OSError:
        return []
    if _san_cache["mtime"] == mtime:
        return list(_san_cache["ips"])
    try:
        out = subprocess.run(
            ["openssl", "x509", "-in", str(CERT_FILE), "-noout", "-ext", "subjectAltName"],
            capture_output=True, text=True, timeout=10,
        ).stdout
        ips = re.findall(r"IP Address:([0-9.]+)", out)
    except Exception:
        ips = []
    _san_cache["mtime"] = mtime
    _san_cache["ips"] = ips
    return list(ips)


# ---------------- WebRTC 信令 ----------------
# 只转手 SDP / ICE 这些几百字节的信令，音视频本身在手机和电脑之间点对点直连，
# 不经过服务器。用 HTTP 长轮询实现，不需要额外依赖。
_rtc_lock = threading.Lock()
_rtc_rooms: dict = {}
RTC_ROOM_TTL = 3 * 3600
RTC_MSG_TTL = 180
RTC_PEER_TTL = 12
RTC_MAX_MSGS = 300


def _rtc_cleanup(now: float) -> None:
    for key in [k for k, v in _rtc_rooms.items() if now - v["born"] > RTC_ROOM_TTL]:
        _rtc_rooms.pop(key, None)


def rtc_send(room: str, role: str, kind: str, data) -> int:
    now = time.time()
    with _rtc_lock:
        _rtc_cleanup(now)
        r = _rtc_rooms.get(room)
        if r is None:
            r = {"seq": 0, "msgs": [], "seen": {}, "born": now}
            _rtc_rooms[room] = r
        r["seq"] += 1
        r["msgs"].append({"seq": r["seq"], "role": role, "kind": kind, "data": data, "t": now})
        if len(r["msgs"]) > RTC_MAX_MSGS:
            del r["msgs"][: len(r["msgs"]) - RTC_MAX_MSGS]
        r["seen"][role] = now
        return r["seq"]


def rtc_poll(room: str, role: str, since: int, wait: float = 8.0) -> dict:
    """长轮询：有对方的消息就立刻返回，没有就挂到 wait 秒再返回空。"""
    deadline = time.time() + wait
    while True:
        now = time.time()
        with _rtc_lock:
            _rtc_cleanup(now)
            r = _rtc_rooms.get(room)
            if r is None:
                return {"ok": False, "error": "no-room", "cursor": since, "msgs": [], "peers": []}
            r["seen"][role] = now
            fresh = [m for m in r["msgs"] if m["seq"] > since and now - m["t"] < RTC_MSG_TTL]
            cursor = fresh[-1]["seq"] if fresh else since
            msgs = [m for m in fresh if m["role"] != role]
            peers = [k for k, v in r["seen"].items() if k != role and now - v < RTC_PEER_TTL]
            if msgs or now >= deadline:
                return {"ok": True, "cursor": cursor, "msgs": msgs, "peers": peers}
        time.sleep(0.1)


class Handler(SimpleHTTPRequestHandler):
    server_version = "MONITOR/1.0"
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    # ---------- auth ----------
    def _authorized(self) -> bool:
        if not CONFIG["auth"]:
            return True
        header = self.headers.get("Authorization", "")
        if not header.startswith("Basic "):
            return False
        try:
            raw = base64.b64decode(header[6:]).decode("utf-8")
        except Exception:
            return False
        return raw == CONFIG["auth"]

    def _require_auth(self) -> bool:
        if self._authorized():
            return False
        body = "需要登录后访问。".encode("utf-8")
        self.send_response(401)
        self.send_header("WWW-Authenticate", 'Basic realm="MONITOR", charset="UTF-8"')
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        return True

    # ---------- routes ----------
    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/api/info":
            return self._api_info()
        if path == "/api/cert":
            return self._api_cert()
        if path == "/api/rtc/poll":
            return self._rtc_poll()
        if self._require_auth():
            return
        return super().do_GET()

    def do_HEAD(self):
        if self._require_auth():
            return
        return super().do_HEAD()

    def do_POST(self):
        if self.path.split("?")[0] == "/api/rtc/send":
            return self._rtc_send()
        self.send_error(405, "method not allowed")

    # ---------- WebRTC 信令 ----------
    def _json_body(self) -> dict:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length <= 0 or length > 65536:
            return {}
        try:
            return json.loads(self.rfile.read(length).decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return {}

    def _send_json(self, payload: dict, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _rtc_send(self):
        body = self._json_body()
        room = str(body.get("room") or "").strip()[:64]
        role = str(body.get("role") or "").strip()[:16]
        kind = str(body.get("kind") or "").strip()[:16]
        if not room or role not in ("phone", "viewer") or not kind:
            return self._send_json({"ok": False, "error": "bad-request"}, 400)
        seq = rtc_send(room, role, kind, body.get("data"))
        return self._send_json({"ok": True, "seq": seq})

    def _rtc_poll(self):
        q = parse_qs(urlparse(self.path).query)
        room = (q.get("room") or [""])[0].strip()[:64]
        role = (q.get("role") or [""])[0].strip()[:16]
        try:
            since = int((q.get("since") or ["0"])[0])
        except ValueError:
            since = 0
        try:
            wait = float((q.get("wait") or ["8"])[0])
        except ValueError:
            wait = 8.0
        wait = max(0.0, min(wait, 15.0))
        if not room or role not in ("phone", "viewer"):
            return self._send_json({"ok": False, "error": "bad-request"}, 400)
        return self._send_json(rtc_poll(room, role, since, wait))

    def _api_info(self):
        # 每次请求都重新探测：换 Wi-Fi / DHCP 重新分配之后，页面刷新就能拿到新地址
        ips = lan_ips()
        CONFIG["lan_ips"] = ips
        scheme = "https" if CONFIG["https"] else "http"
        port = CONFIG["https_port"] if CONFIG["https"] else CONFIG["http_port"]

        # 优先沿用「浏览器正在用的这个主机名」：从手机访问时拿到的就是手机
        # 当前真正连得通的地址，而不是电脑猜出来的第一个网卡地址。
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0].strip("[]")
        if is_loopback(host):
            host = ips[0] if ips else "127.0.0.1"
        elif ips and host not in ips:
            ips.insert(0, host)
        share = f"{scheme}://{host}:{port}/"
        # 手机能用的地址：永远是局域网 IP，不能是 localhost（手机上 localhost 指向手机自己）
        lan_host = host if not is_loopback(host) else (ips[0] if ips else "127.0.0.1")
        lan_share = f"{scheme}://{lan_host}:{port}/"

        payload = {
            "shareUrl": share,
            "lanShareUrl": lan_share,
            "host": host,
            "lanHost": lan_host,
            "scheme": scheme,
            "port": port,
            "lanIps": ips,
            "certIps": cert_san_ips(),
            "auth": bool(CONFIG["auth"]),
            "tls": CONFIG["https"],
        }
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _api_cert(self):
        if not CERT_FILE.exists():
            self.send_error(404, "no certificate")
            return
        data = CERT_FILE.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "application/x-x509-ca-cert")
        self.send_header("Content-Disposition", 'attachment; filename="monitor-local-cert.pem"')
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    # ---------- misc ----------
    def end_headers(self):
        if self.path.startswith("/api/"):
            self.send_header("Cache-Control", "no-store")
        elif self.path.endswith((".html", ".js", ".css")):
            self.send_header("Cache-Control", "no-cache")
        super().end_headers()

    def log_message(self, fmt, *args):
        message = fmt % args
        if "/api/info" in message:
            return
        sys.stderr.write("  %s — %s\n" % (time.strftime("%H:%M:%S"), message))


class RedirectHandler(SimpleHTTPRequestHandler):
    """把 HTTP 请求重定向到 HTTPS。"""

    def do_GET(self):
        host = self.headers.get("Host", "").split(":")[0] or "localhost"
        target = f"https://{host}:{CONFIG['https_port']}{self.path}"
        self.send_response(302)
        self.send_header("Location", target)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_HEAD = do_GET

    def log_message(self, fmt, *args):
        pass


class Server(ThreadingHTTPServer):
    """TLS 上下文可以热替换，换网络重签证书后无需重启。"""

    daemon_threads = True

    def __init__(self, addr, handler, ctx=None):
        self.ssl_ctx = ctx
        super().__init__(addr, handler)

    def handle_error(self, request, client_address):
        # 手机锁屏、切后台、断开 Wi-Fi 都会让连接直接消失，这类错误不值得打印堆栈
        exc = sys.exc_info()[1]
        if isinstance(exc, (ConnectionResetError, BrokenPipeError, ssl.SSLError, TimeoutError)):
            return
        super().handle_error(request, client_address)

    def get_request(self):
        sock, addr = super().get_request()
        ctx = self.ssl_ctx
        if ctx is None:
            return sock, addr
        try:
            return ctx.wrap_socket(sock, server_side=True), addr
        except (ssl.SSLError, OSError):
            # 握手失败（例如浏览器直接以 http 打过来）就丢掉这条连接
            try:
                sock.close()
            except OSError:
                pass
            raise


def make_ctx() -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(CERT_FILE), str(KEY_FILE))
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    return ctx


def serve(handler, port: int, tls: bool) -> Server:
    return Server(("0.0.0.0", port), handler, make_ctx() if tls else None)


def watch_network(https_srv: Server | None, stop: threading.Event) -> None:
    """盯着网络地址：一变就重签证书并让监听用上新证书。"""
    last = tuple(lan_ips())
    while not stop.wait(15):
        try:
            ips = lan_ips()
        except Exception:
            continue
        CONFIG["lan_ips"] = ips
        if tuple(ips) == last:
            continue
        last = tuple(ips)
        shown = ", ".join(ips) if ips else "（未检测到局域网地址）"
        if https_srv is None:
            print(f"· 网络地址已变化 → {shown}")
            continue
        try:
            ensure_cert()
            https_srv.ssl_ctx = make_ctx()
            print(f"· 网络地址已变化 → {shown}（证书已更新，刷新页面即可）")
        except Exception as exc:  # noqa: BLE001
            print(f"! 网络地址已变化 → {shown}，但更新证书失败：{exc}")


def banner(use_tls: bool, https_port: int, http_port: int, auth) -> None:
    line = "─" * 52
    print("\n" + line)
    print("  MONITOR · 摄像头实时取景")
    print(line)
    print(f"  本机访问   http://localhost:{http_port}/")
    if use_tls:
        print(f"  本机 HTTPS https://localhost:{https_port}/")
        ips = CONFIG["lan_ips"]
        if ips:
            for ip in ips:
                print(f"  手机访问   https://{ip}:{https_port}/")
            print("\n  以上地址是自动检测的当前局域网地址；换 Wi-Fi 后会重新检测，")
            print("  刷新页面上的二维码即可拿到新地址。")
        else:
            print("  手机访问   未检测到局域网地址（请先连上 Wi-Fi 或网线）")
        print("\n  提示：手机首次打开会提示「证书不受信任」，")
        print("        选择「继续访问 / 高级 → 继续」即可。")
        print("        也可在手机浏览器打开 /api/cert 下载证书后安装信任。")
    else:
        print("\n  注意：未启用 HTTPS，手机通过局域网访问将无法使用摄像头。")
        print("        浏览器只允许 HTTPS 或 localhost 访问摄像头。")
    if auth:
        print(f"\n  访问口令   账号 {auth.split(':')[0]} / 密码 {auth.split(':', 1)[1]}")
    print(line + "\n")


def main() -> None:
    # 输出重定向到文件/管道时默认是全缓冲，这里改成行缓冲，保证提示信息立刻可见
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except (AttributeError, ValueError):
        pass

    ap = argparse.ArgumentParser(description="MONITOR 本地摄像头服务器")
    ap.add_argument("--port", type=int, default=8443, help="HTTPS 端口（默认 8443）")
    ap.add_argument("--http-port", type=int, default=8080, help="HTTP 端口（默认 8080，用于跳转）")
    ap.add_argument("--no-tls", action="store_true", help="只使用 HTTP")
    ap.add_argument("--auth", default=os.environ.get("MONITOR_AUTH"), help="Basic 认证，格式 user:pass")
    ap.add_argument("--open", action="store_true", help="启动后自动打开浏览器")
    args = ap.parse_args()

    use_tls = not args.no_tls
    CONFIG["https"] = use_tls
    CONFIG["https_port"] = args.port
    CONFIG["http_port"] = args.http_port
    CONFIG["auth"] = args.auth
    CONFIG["lan_ips"] = lan_ips()

    if use_tls:
        ensure_cert()

    banner(use_tls, args.port, args.http_port, args.auth)

    servers = []
    https_srv = None
    if use_tls:
        https_srv = serve(Handler, args.port, True)
        servers.append(https_srv)
        try:
            http_srv = serve(RedirectHandler, args.http_port, False)
            servers.append(http_srv)
        except OSError as exc:
            print(f"! HTTP 跳转端口 {args.http_port} 启动失败：{exc}")
        url = f"https://localhost:{args.port}/"
    else:
        servers.append(serve(Handler, args.http_port, False))
        url = f"http://localhost:{args.http_port}/"

    threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in servers]
    for t in threads:
        t.start()

    stop_watch = threading.Event()
    threading.Thread(target=watch_network, args=(https_srv, stop_watch), daemon=True).start()

    if args.open:
        threading.Timer(0.8, lambda: webbrowser.open(url)).start()

    print(f"  服务已启动 → {url}\n  按 Ctrl+C 停止\n")
    try:
        while True:
            time.sleep(0.5)
    except KeyboardInterrupt:
        print("\n  正在停止…")
    finally:
        stop_watch.set()
        for s in servers:
            s.shutdown()
            s.server_close()


if __name__ == "__main__":
    main()
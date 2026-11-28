# MONITOR · Tauri 前后端契约（冻结版 v1）

前端：静态文件 `ui/`（无构建步骤）。通过 `window.__TAURI__.core.invoke` 调命令，
通过 `window.__TAURI__.event.listen` 收事件（tauri.conf.json 已开 withGlobalTauri）。

## 1. 命令（invoke）

| 命令 | 入参 | 返回 |
| --- | --- | --- |
| `app_info` | — | `{ streamUrl: string, previewUrl: string, mediaBase: string, version: string }` |
| `list_devices` | — | `{ av: {index:number,name:string}[], android: {serial:string,model:string}[], ffmpegOk: boolean, adbOk: boolean }` |
| `start_source` | `{ kind: "av"\|"android"\|"wireless", id: string }` | `{ gen: number, state: "starting" }` |
| `stop_source` | — | `{ ok: true }` |
| `take_photo` | `{ mirror: boolean }` | `{ name, path, url, thumbUrl }`（失败抛出字符串错误） |
| `start_recording` | — | `{ name, path }` |
| `stop_recording` | — | `{ name, path, secs, thumbUrl, error: string\|null }` |
| `recording_status` | — | `{ recording: boolean, secs: number, name: string\|null }` |
| `pair_info` | — | `{ url: string, qrUrl: string, lan: string, port: number, token: string, available: boolean, hint: string }` |
| `pair_stop` | — | `{ ok: true }` |
| `open_path` | `{ path: string, reveal: boolean }` | `{ ok: true }` |
| `open_captures_dir` | — | `{ ok: true, dir: string }` |
| `delete_capture` | `{ name: string }` | `{ ok: true }` |
| `captures_dir` | — | `{ dir: string }` |

- `id`：av 用设备序号字符串（"0"）；android 用 serial；wireless 固定 `"phone"`。
- `take_photo` / `start_recording` 在没有画面时抛错（中文提示）。
- `url` / `thumbUrl` 形如 `http://127.0.0.1:PORT/media/IMG_xxx.jpg`、`.../thumb/IMG_xxx.jpg`；
  视频 thumbUrl 为 `.../thumb/VID_xxx.jpg`（首帧 JPEG）。
- `pair_info`：首次调用会启动手机无线服务；`available=false` 时 `hint` 说明原因（无局域网/证书失败）。
  `qrUrl` 为可直接给 `<img src>` 的 PNG 地址。

## 2. 事件（listen）

| 事件名 | payload |
| --- | --- |
| `source-state` | `{ gen:number, state:"starting"\|"live"\|"error"\|"stopped", detail: string\|null }` |
| `frame-info` | `{ gen:number, width:number, height:number, fps:number }`（约每秒 1 次） |
| `record-tick` | `{ secs:number }`（录像时每秒） |
| `wireless` | `{ connected:boolean, name:string\|null, hint:string\|null }` |
| `notice` | `{ text:string, level:"info"\|"error" }` |

## 3. 画面传输：本地 WebSocket + 单帧长轮询

`app_info.previewUrl` 指向 `ws://127.0.0.1:PORT/preview?t=<本进程令牌>`。升级请求须携带令牌及应用 Origin（WKWebView 的 `null` 需同样通过令牌校验）。客户端发 `next`，服务端最多等 1 秒后回最新的 JPEG 二进制帧，或者回文本 `idle` 表示无新帧。客户端处理完成再发下一条 `next`，慢消费者不累积旧帧。

`app_info.streamUrl` 仍指向 `/stream`（兼容旧客户端）。WebSocket 不可用时从同地址的 `/frame.jpg` 读取，每次响应都是一张完整 JPEG。

请求带两个查询参数：

- `v`：上一次拿到的帧号（来自响应头 `X-Frame-Seq`）。首次请求可以不带，或带 `-1`；解析不出数字时服务端按「没有旧帧」处理，立刻返回当前最新帧。
- `wait`：最长挂起毫秒数，服务端上限 2000，前端用 250。

响应：

- `200` + `image/jpeg`：头里带 `X-Frame-Seq: <n>`（单调递增）与 CORS 头（`Access-Control-Allow-Origin: *`、`Access-Control-Expose-Headers: X-Frame-Seq`），前端把 `n` 写进下一次的 `v`。
- `204 No Content`：带了 `v`，但 `wait` 内没有新帧。这不是错误，前端立刻发起下一次等待。
- `404`：当前没有任何帧（还没连上源、或已停止），前端退避约 120ms 再试。

前端在背面 `<img>` 解码，成功后才切到前面；坏帧及短暂断线保留上一帧。切源时关闭 WebSocket、取消在途 HTTP，并丢弃旧连接的迟到结果。

**不要**把这里改回 `fetch` + `multipart/x-mixed-replace`（`/stream`）：WKWebView 打开该流后第一次 `read()` 直接 `TypeError: Load failed`，一帧都收不到。`/stream` 端点仅为 curl、测试与旧客户端保留。

## 4. 其他 HTTP 端点（只读）

- `GET /media/<name>` → 照片 JPEG / 录像 MP4（支持 Range，供 `<video>` 播放）。
- `GET /thumb/<name>` → 160px 高缩略图 JPEG（视频为首帧）。
- `GET /qr` → PNG 二维码（配对用）。

## 5. 目录约定

- `ui/` 仅前端静态资源与测试；`ui/tests/*.test.mjs` 用 `node --test` 运行，纯 ESM、零依赖。
- 纯逻辑（比例布局、MJPEG 解析、格式化）必须抽成可 import 的模块以便测试。
## 6. 实现状态

- 后端（`src-tauri/src/`）：全部命令与事件已实现，`cargo test` 39 项通过（含真实 ffmpeg 录像、MJPEG 流、HTTP Range、HTTPS + WSS 手机推流端到端）。
- 前端（`ui/`）：静态实现，`node --test ui/tests/*.test.mjs` 46 项通过。
- 打包：`pnpm tauri build` 产出 `target/release/bundle/macos/MONITOR.app`。


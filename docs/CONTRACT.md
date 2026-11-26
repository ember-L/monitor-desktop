# MONITOR · Tauri 前后端契约（冻结版 v1）

前端：静态文件 `ui/`（无构建步骤）。通过 `window.__TAURI__.core.invoke` 调命令，
通过 `window.__TAURI__.event.listen` 收事件（tauri.conf.json 已开 withGlobalTauri）。

## 1. 命令（invoke）

| 命令 | 入参 | 返回 |
| --- | --- | --- |
| `app_info` | — | `{ streamUrl: string, mediaBase: string, version: string }` |
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

## 3. 画面传输：本地 MJPEG-over-fetch

`app_info.streamUrl` = `http://127.0.0.1:PORT/stream`（每帧带 Content-Length 的 multipart）。

前端用 `fetch(streamUrl)` + `response.body.getReader()` 解析：

```
--frame\r\n
Content-Type: image/jpeg\r\n
Content-Length: <N>\r\n
\r\n
<N 字节 JPEG>
\r\n
```

解析规则（必须容错）：
1. 累积字节到缓冲区，用 `\r\n\r\n` 分隔头部与载荷；
2. 头部缺 Content-Length 时，回退到按 JPEG SOI(`FF D8`) / EOI(`FF D9`) 扫描；
3. 载荷读满 N 字节后即为一帧，交给 `createImageBitmap(new Blob([bytes],{type:"image/jpeg"}))`，
   `ctx.drawImage(bitmap, ...)` 到画布，`bitmap.close()`；
4. 解码失败跳过该帧，不断流；
5. 连接断开（源切换/暂停）后在 300ms 后重连；切源时 `gen` 变化应立刻重连并丢弃旧帧。

**不要**用 `<img src=".../stream">`（WebKit 不支持 multipart MJPEG）。

## 4. 其他 HTTP 端点（只读）

- `GET /media/<name>` → 照片 JPEG / 录像 MP4（支持 Range，供 `<video>` 播放）。
- `GET /thumb/<name>` → 160px 高缩略图 JPEG（视频为首帧）。
- `GET /qr` → PNG 二维码（配对用）。

## 5. 目录约定

- `ui/` 仅前端静态资源与测试；`ui/tests/*.test.mjs` 用 `node --test` 运行，纯 ESM、零依赖。
- 纯逻辑（比例布局、MJPEG 解析、格式化）必须抽成可 import 的模块以便测试。
## 6. 实现状态

- 后端（`src-tauri/src/`）：全部命令与事件已实现，`cargo test` 27 项通过（含真实 ffmpeg 录像、MJPEG 流、HTTP Range、HTTPS + WSS 手机推流端到端）。
- 前端（`ui/`）：静态实现，`node --test ui/tests/*.test.mjs` 46 项通过。
- 打包：`pnpm tauri build` 产出 `target/release/bundle/macos/MONITOR.app`。


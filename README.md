# MONITOR · Tauri 桌面取景器

把手机屏幕 / 电脑摄像头变成桌面上的实时取景器：**Android（adb + scrcpy 协议）、iPhone / iPad（USB）、本机摄像头、手机无线推流（扫码）**自由切换；屏幕比例预设 + 自定义（fit / fill / stretch）；拍照、录像、胶片条、网格、镜像一应俱全。全部本地完成，画面不上传任何服务器。

桌面外壳已从 eframe/egui 0.31 重写为 **Tauri 2**（Rust 后端 + WebView 前端，前端无打包步骤）。旧的网页版仍留在 `web/`，见文末。

## 快速开始

```bash
# 依赖：Rust 工具链、Node.js（仅安装打包 CLI 与跑前端测试）、ffmpeg；Android 另需 adb
brew install ffmpeg
# adb：安装 Android platform-tools，或 brew install --cask android-platform-tools

pnpm install            # 只装 @tauri-apps/cli，ui/ 是纯静态文件
pnpm build              # 发布构建 + 生成 .app（自动探测 Rust 工具链）
open target/release/bundle/macos/MONITOR.app
```

macOS 也可以直接双击 **`启动.command`**（首次自动 `cargo build --release` 并启动 `target/release/monitor-desktop`）。

未签名的 .app 首次打开可能被 Gatekeeper 拦下：右键 →「打开」，或 `xattr -dr com.apple.quarantine target/release/bundle/macos/MONITOR.app`。

工具链装在项目内 `.rust/` 目录也能用（`RUSTUP_HOME` / `CARGO_HOME` 指过去即可），启动脚本会自动识别。

`pnpm` 脚本经 `scripts/tauri.mjs` 包装：先找到 cargo（`PATH`、`~/.cargo/bin`、项目内 `.rust/cargo`、`~/.rustup/toolchains/*`），把它的目录补进 `PATH`，再调用 Tauri CLI。若看到 `failed to run 'cargo metadata' ... No such file or directory`，说明当前 shell 的 `PATH` 里没有 cargo——改用 `pnpm build` / `pnpm dev`，或手动把 cargo 目录加进 `PATH`。排查可用 `MONITOR_PRINT_TOOLCHAIN=1 pnpm tauri --version` 打印探测到的工具链。

## 设备源

| 源 | 接入方式 | 说明 |
| --- | --- | --- |
| 本机摄像头 | ffmpeg avfoundation | 设备列表即 macOS 采集设备列表；自动探测可用像素格式（默认 / uyvy422 / nv12） |
| iPhone / iPad | USB + ffmpeg avfoundation | 数据线连接、解锁并「信任此电脑」后，iPhone 会作为视频采集设备出现在列表里 |
| Android | adb + 内嵌 scrcpy-server v3.1 | 开启 USB 调试并授权；自动推送内嵌 server（`src-tauri/assets/scrcpy-server-v3.1`，Apache-2.0）、建立 forward、按 scrcpy 视频协议取 H.264 流并用 ffmpeg 转 JPEG |
| 手机无线 | 同一 Wi-Fi + 扫码 | 点「无线」弹出二维码；手机扫码打开页面即用摄像头推流（见下节） |

左侧设备面板每 10 秒自动刷新，也可点「刷新」。选中即切换画面；切换时旧源自动停止并清理（adb forward / 设备端 server / 无线会话）。

找不到工具时状态栏会给出提示；也可以用环境变量指定路径：`MONITOR_FFMPEG`、`MONITOR_ADB`。

## 手机无线推流（扫码）

1. 电脑与手机连同一个 Wi-Fi；
2. 点界面顶部「无线」→ 应用在 `0.0.0.0` 的随机端口起一个 HTTPS / WSS 服务（启动时用 rustls + rcgen 现生成自签证书），并显示二维码 `https://<局域网 IP>:<端口>/?t=<一次性 token>`；
3. 手机扫码打开页面 → 允许使用摄像头 → 页面把画面以 JPEG 经 WSS 推给桌面，作为「无线」设备源实时显示；
4. 手机上可切换前后摄像头、分辨率（640 / 960 / 1280）、帧率（15 / 24 / 30）、是否镜像；token 每次配对唯一，点「断开配对」立即失效。

首次打开时浏览器会提示证书不受信任（自签），选择「继续访问」即可。整条链路只在局域网内，画面不经过任何服务器。

## 屏幕比例

- 预设：跟随源 / 16:9 / 9:16 / 4:3 / 3:4 / 3:2 / 2:3 / 21:9 / 1:1；
- 自定义：任意正数宽高比；
- 三种填充模式：**适应（fit）** 完整画面留黑边、**裁切（fill）** 居中裁切填满、**拉伸（stretch）** 变形填满。

## 功能与快捷键

| 功能 | 说明 |
| --- | --- |
| 实时取景 | 叠加时间码 / 分辨率 / FPS；源画面尺寸变化自动跟随 |
| 拍照（空格） | 当前帧存为 JPEG（跟随镜像开关），快门闪光反馈 |
| 录像（R） | ffmpeg 编码 H.264 MP4，实时显示时长；源分辨率变化自动缩放到录制尺寸 |
| 胶片条 | 底部缩略图：点击放大查看、打开、在 Finder 中显示、删除（两步确认） |
| 网格（G） | 三分构图网格 |
| 镜像（M） | 预览与拍照同步镜像 |
| 全屏（F） | 全屏取景 |

照片 / 视频保存在 `~/Movies/MONITOR`，界面上「打开素材目录」直达。

## 开发

```bash
pnpm dev                             # 开发模式（ui/ 由 WebView 直接加载，改前端刷新即生效）
cargo test                           # Rust 单元 / 集成测试
node --test ui/tests/*.test.mjs      # 前端逻辑测试
pnpm build                           # 发布构建
```

目录：

```
src-tauri/
  src/main.rs      Tauri 命令、事件泵、窗口
  src/source.rs    设备枚举与取景源（摄像头 / Android / 无线），JPEG 帧管线
  src/ffmpeg.rs    ffmpeg 定位、进程超时、JPEG 尺寸解析
  src/android.rs   adb 设备解析
  src/devices.rs   avfoundation 设备解析
  src/capture.rs   拍照（JPEG）/ 录像（MP4）/ 缩略图
  src/viewer.rs    本地 127.0.0.1 取景服务：MJPEG /stream、/media、/thumb、/qr
  src/wireless.rs  局域网 HTTPS + WSS 推流、配对 token、二维码
  src/http.rs      手写 HTTP/1.1 与 WebSocket 帧协议
  src/phone_page.html  手机端页面（编进二进制）
  assets/scrcpy-server-v3.1
ui/                纯静态前端（index.html / styles.css / app.js / mjpeg.js / layout.js / format.js）
docs/CONTRACT.md   前后端命令 / 事件 / 流协议契约
docs/ui-spec.md    界面与交互规范
```

画面链路：ffmpeg 或手机端直接产出 JPEG，后端只保留最新一帧，经本地 MJPEG HTTP 流送进 WebView（WebKit 不支持 multipart `<img>` 流，前端用 `fetch` + `ReadableStream` 自己拆帧）；拍照直接落盘 JPEG，录像时才解码成 rawvideo 交给 libx264。

## 网页版（旧）

`web/` 是重构前更早的浏览器版（WebRTC + `serve.py` + 自签证书），仅作视觉与交互参考，与当前桌面端无构建关系。

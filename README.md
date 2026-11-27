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
3. 手机扫码打开页面 → 允许使用摄像头 → 页面**自动开始推流**（每帧一张 JPEG），作为「无线」设备源实时显示；打开页面后无需再点任何按钮，想暂停再点页面上的「暂停发送」；
4. 手机上可切换前后摄像头、分辨率（640 / 960 / 1280）、帧率（15 / 24 / 30）、是否镜像；token 每次配对唯一，点「断开配对」立即失效。

首次打开时浏览器会提示证书不受信任（自签），选择「继续访问」即可。整条链路只在局域网内，画面不经过任何服务器。

### 手机打不开页面 / 没有画面时

配对浮层每 1.5 秒刷新一次诊断状态，直接说明手机走到了哪一步：

| 浮层显示 | 含义 | 下一步 |
| --- | --- | --- |
| 手机还没有连过来 | 手机根本没连到这端口 | 确认手机与电脑同一个 Wi‑Fi（浮层上方的地址可以从手机浏览器手动打开）；用手机「相机」或 Safari 扫码——微信 / QQ 内置浏览器不信任本地证书；微信扫码后点右上角「…」→「在 Safari 中打开」 |
| 手机连到了电脑，但页面没打开 | TLS 握手通了，页面没取到 | 在手机的证书警告页点「显示详细信息」→「访问此网站」 |
| 配对码不对，页面没打开 | 手机用的是旧二维码 | 电脑上点「刷新地址」换新 token，再重扫 |
| 手机打开了页面，但还没推流 | 页面开了，画面没上来 | 允许使用摄像头，并在页面上点「开始发送」 |
| 手机已连上，但还没收到画面 | 有连接没帧 | 同上；页面切到后台会暂停推流 |
| 手机已连接，画面传输中 | 链路正常 | 桌面没画面就重新选一次左侧「无线（扫码）」 |

仍然不行时看诊断日志（只记连接与请求行，不记画面内容）：`~/Library/Logs/MONITOR/wireless.log`；该目录写不进去时退到 `/tmp/monitor-wireless.log`。

上行通道有两条，页面会自动选：优先用 WSS 推流；若手机浏览器不接受自签证书导致 WSS 握手失败（iOS Safari 常见），页面会在同一端口改用同源 HTTPS 逐帧 POST 上传，画面照样能到桌面，只是帧率略低。想彻底避免证书提示，可在手机页或浏览器里打开 `/cert` 下载并信任该证书（macOS 也可用 `security add-trusted-cert`）。桌面端已连上但 6 秒内没收到画面时，会提示「手机已连接，但还没收到画面」，多数情况是手机页的相机权限没给或页面被切到后台。

手机锁屏或切到别的 App 时，系统会冻结页面、暂停推流：页面回到前台会自动重建上行通道（WSS 连接假死则直接换一条新连接；逐帧 POST 每帧带 8 秒超时，卡住会自行重试），另有看门狗在连续 6 秒没送出画面时强制重启推流。桌面端连续 4 秒没收到帧，会把状态改成「等待手机画面」，不再让最后一帧冒充实时画面；手机恢复推流后自动回到实时状态。

二维码里的地址始终来自当前网卡：换了 Wi-Fi、插上网线或开关 VPN 之后，点浮层里的「刷新地址」即可重新读取本机 IP（切回窗口时也会自动刷新一次）。电脑有多块网卡（比如同时连着 Wi-Fi 和虚拟网卡）时，「本机 IP」会变成下拉框，选手机真正能访问到的那个地址即可；二维码、端口与 token 都保持不变。

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
| 放大取景（F / 画面右下角 ⌕ 按钮） | 藏起顶栏、侧栏与胶片条，画面铺满窗口并进入系统全屏；再按 F 或 Esc 退出 |

照片 / 视频保存在 `~/Movies/MONITOR`（Windows 是 `%USERPROFILE%\Videos\MONITOR`），界面上「打开素材目录」直达。

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
  src/devices.rs   相机枚举（macOS avfoundation / Windows DirectShow）
  src/capture.rs   拍照（JPEG）/ 录像（MP4）/ 缩略图
  src/viewer.rs    本地 127.0.0.1 取景服务：单帧 /frame.jpg（界面用这个）、MJPEG /stream、/media、/thumb、/qr
  src/wireless.rs  局域网 HTTPS + WSS 推流、配对 token、二维码
  src/http.rs      手写 HTTP/1.1 与 WebSocket 帧协议
  src/phone_page.html  手机端页面（编进二进制）
  assets/scrcpy-server-v3.1
ui/                纯静态前端（index.html / styles.css / app.js / mjpeg.js / layout.js / format.js）
docs/CONTRACT.md   前后端命令 / 事件 / 流协议契约
docs/ui-spec.md    界面与交互规范
```

画面链路：ffmpeg 或手机端直接产出 JPEG，后端只保留最新一帧，前端用 `fetch` 轮询本地 `GET /frame.jpg` 取单帧、`createImageBitmap` 解码后画到 canvas（每帧一个小请求，30 fps 左右）。**不要改回 multipart 长连接流**：WKWebView 打开 `multipart/x-mixed-replace` 的 `fetch` 后第一次 `read()` 直接 `TypeError: Load failed`，桌面端会一帧都收不到（`/stream` 端点仍保留给 curl、测试和旧客户端）。拍照直接落盘 JPEG，录像时才解码成 rawvideo 交给 libx264。

手机无线画面防闪黑：手机页把每帧缩到 16×9 估平均亮度，抓到的全黑帧（安卓上偶发的空 canvas）直接丢掉并计入页面上的「跳过」；桌面端收到帧后同样再挡一次（16×9 缩略图平均亮度低于 4 就不上屏，`ui/app.js` 的 `isBlackFrame`）。手机页的发送循环是串行的：抓完一帧发出去再抓下一帧，避免多帧并发让旧帧后到，画面忽明忽暗。

## Windows 打包

Windows 安装包（NSIS `.exe` + MSI）必须在 Windows 上构建：Tauri 打包器要调用 Windows 侧工具链（WiX / NSIS），macOS 上无法交叉生成安装包。代码已按平台适配：相机走 DirectShow，局域网 IP 走 `ipconfig`，素材目录是 `%USERPROFILE%\Videos\MONITOR`，用 `explorer` 打开目录，没有 `/dev/urandom` 时用混合随机源生成配对码。

三种做法，任选其一：

1. **GitHub Actions**（最省事）：推到 GitHub 后手动触发 `build-windows`（见 `.github/workflows/windows.yml`），产物在 Actions 的 `monitor-windows` 工件里（`MONITOR_1.0.0_x64-setup.exe`、`.msi`，以及免安装的 `monitor-desktop.exe`）。
2. **Windows 机器本地构建**：装 Rust（MSVC 工具链）、Node 20+、pnpm，然后：

   ```powershell
   pnpm install
   pnpm build      # 产物在 target\release\bundle\
   ```

3. **macOS 上交叉编译校验**（只验证能编译 / 产出免安装 exe，不产出安装包）：

   ```bash
   rustup target add x86_64-pc-windows-msvc
   cargo install cargo-xwin
   cargo xwin build --release --target x86_64-pc-windows-msvc
   cargo xwin check --target x86_64-pc-windows-msvc   # 只做类型检查，最快
   ```

   交叉编译要求本机有 LLVM 的 `llvm-rc` / `lld-link`（Tauri 要往 exe 里写图标和版本资源）；缺了会在 `tauri-winres` 处报 `NotAttempted("llvm-rc")`，此时只能用第 1、2 种做法。

应用启动后不会弹 cmd 黑框：ffmpeg / adb / `ipconfig` / `where` 这些子进程统一走 `src-tauri/src/proc.rs` 的 `command()` 创建，Windows 上带 `CREATE_NO_WINDOW`。新加子进程也要用它，别直接 `Command::new`。

安装包没有代码签名，Windows SmartScreen 会拦一次：点「更多信息」→「仍要运行」即可。

Windows 上还需要 ffmpeg 才能用本机摄像头和 Android 投屏（手机扫码的无线取景不需要）：`winget install Gyan.FFmpeg`，或把 `ffmpeg.exe` 放到应用目录，或设置 `MONITOR_FFMPEG` 指向它；Android 需 platform-tools（`%LOCALAPPDATA%\Android\Sdk\platform-tools\adb.exe`）。WebView2 缺失时安装包会自动下载安装。

## 网页版（旧）

`web/` 是重构前更早的浏览器版（WebRTC + `serve.py` + 自签证书），仅作视觉与交互参考，与当前桌面端无构建关系。

//! MONITOR 桌面端：Tauri 2 外壳 + 本地 MJPEG 取景服务 + 手机无线配对。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod android;
mod capture;
mod devices;
mod ffmpeg;
mod http;
mod proc;
mod source;
mod viewer;
mod wireless;

use serde::Serialize;
use source::{DeviceId, Event, Frame, SourceHandle, SourceState};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use viewer::Viewer;
use wireless::Wireless;

struct Core {
    gen: u64,
    current: Option<DeviceId>,
    source: Option<SourceHandle>,
    frame: Option<Frame>,
    recorder: Option<capture::Recorder>,
    fps_mark: Instant,
    fps_frames: u32,
    fps_value: f32,
}

struct App {
    viewer: Arc<Viewer>,
    wireless: Arc<Wireless>,
    tx: Sender<Event>,
    core: Mutex<Core>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    stream_url: String,
    media_base: String,
    version: String,
}

#[derive(Serialize)]
struct StartResult {
    gen: u64,
    state: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhotoResult {
    name: String,
    path: String,
    url: String,
    thumb_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordStart {
    name: String,
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordStop {
    name: String,
    path: String,
    secs: f64,
    thumb_url: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordStatus {
    recording: bool,
    secs: f64,
    name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairResult {
    url: String,
    qr_url: String,
    lan: String,
    ips: Vec<String>,
    port: u16,
    token: String,
    available: bool,
    hint: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CapturesDir {
    dir: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SourceStatePayload {
    gen: u64,
    state: String,
    detail: Option<String>,
}

#[derive(Serialize, Clone)]
struct FrameInfoPayload {
    gen: u64,
    width: u32,
    height: u32,
    fps: f32,
}

#[derive(Serialize, Clone)]
struct RecordTickPayload {
    secs: f64,
}

#[derive(Serialize, Clone)]
struct WirelessPayload {
    connected: bool,
    name: Option<String>,
    hint: Option<String>,
}

#[derive(Serialize, Clone)]
struct NoticePayload {
    text: String,
    level: String,
}

#[tauri::command]
fn app_info(app: State<'_, Arc<App>>) -> AppInfo {
    #[cfg(debug_assertions)]
    eprintln!("[monitor] app_info 已由界面调用 → {}", app.viewer.base());
    AppInfo {
        stream_url: app.viewer.url("/stream"),
        media_base: app.viewer.base(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[tauri::command]
async fn list_devices() -> Result<source::DeviceLists, String> {
    tauri::async_runtime::spawn_blocking(source::enumerate_devices)
        .await
        .map_err(|e| format!("枚举设备失败：{e}"))
}

#[tauri::command]
fn start_source(app: State<'_, Arc<App>>, kind: String, id: String) -> Result<StartResult, String> {
    let device = DeviceId::parse(&kind, &id).ok_or("未知的视频源类型")?;
    let (gen, app2) = {
        let mut core = app.core.lock().map_err(|_| "内部状态异常")?;
        if let Some(mut old) = core.source.take() {
            old.stop();
        }
        if let Some(rec) = core.recorder.take() {
            let _ = rec.finish();
        }
        app.viewer.clear();
        core.gen += 1;
        core.frame = None;
        core.current = Some(device.clone());
        core.fps_mark = Instant::now();
        core.fps_frames = 0;
        core.fps_value = 0.0;
        (core.gen, app.inner().clone())
    };

    let tx = app2.tx.clone();
    let viewer = app2.viewer.clone();
    let wireless_svc = app2.wireless.clone();
    let device2 = device.clone();
    std::thread::spawn(move || {
        let result: Result<SourceHandle, String> = match device2 {
            DeviceId::Av(index) => source::spawn_camera(gen, index, tx.clone()),
            DeviceId::Android(serial) => source::spawn_android(gen, &serial, tx.clone()),
            DeviceId::Wireless => {
                let info = wireless_svc.ensure_started();
                if !info.available {
                    Err(info.hint.clone())
                } else if !info.qr.is_empty() {
                    viewer.set_qr(info.qr.clone());
                    source::spawn_wireless(gen, tx.clone(), wireless_svc.clone())
                } else {
                    source::spawn_wireless(gen, tx.clone(), wireless_svc.clone())
                }
            }
        };
        let mut core = match app2.core.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if core.gen != gen {
            return; // 已被新的源取代，句柄 drop 时自动停止
        }
        match result {
            Ok(handle) => core.source = Some(handle),
            Err(err) => {
                let _ = app2.tx.send(Event::State { gen, state: SourceState::Error(err) });
            }
        }
    });

    Ok(StartResult { gen, state: "starting" })
}

#[tauri::command]
fn stop_source(app: State<'_, Arc<App>>) -> Result<serde_json::Value, String> {
    let mut core = app.core.lock().map_err(|_| "内部状态异常")?;
    core.gen += 1;
    if let Some(mut s) = core.source.take() {
        s.stop();
    }
    if let Some(rec) = core.recorder.take() {
        let _ = rec.finish();
    }
    core.frame = None;
    core.current = None;
    app.viewer.clear();
    app.wireless.clear_sink();
    Ok(serde_json::json!({ "ok": true }))
}

#[tauri::command]
async fn take_photo(app: State<'_, Arc<App>>, mirror: bool) -> Result<PhotoResult, String> {
    let frame = {
        let core = app.core.lock().map_err(|_| "内部状态异常")?;
        core.frame.clone().ok_or("当前没有画面，请先连接视频源")?
    };
    let dir = capture::captures_dir();
    let (name, path, thumb) =
        tauri::async_runtime::spawn_blocking(move || capture::save_photo(&frame, mirror, &dir))
            .await
            .map_err(|e| format!("拍照失败：{e}"))??;
    app.viewer.put_thumb(&name, thumb.jpeg);
    Ok(PhotoResult {
        url: app.viewer.url(&format!("/media/{name}")),
        thumb_url: app.viewer.url(&format!("/thumb/{name}")),
        name,
        path: path.to_string_lossy().to_string(),
    })
}

#[tauri::command]
fn start_recording(app: State<'_, Arc<App>>) -> Result<RecordStart, String> {
    let mut core = app.core.lock().map_err(|_| "内部状态异常")?;
    if core.recorder.is_some() {
        return Err("已在录像中".to_string());
    }
    let frame = core.frame.clone().ok_or("当前没有画面，请先连接视频源")?;
    let fps = core.fps_value;
    let rec = capture::start_recording(&frame, fps, &capture::captures_dir())?;
    let out = RecordStart { name: rec.name.clone(), path: rec.path.to_string_lossy().to_string() };
    core.recorder = Some(rec);
    Ok(out)
}

#[tauri::command]
async fn stop_recording(app: State<'_, Arc<App>>) -> Result<RecordStop, String> {
    let rec = {
        let mut core = app.core.lock().map_err(|_| "内部状态异常")?;
        core.recorder.take().ok_or("当前不在录像")?
    };
    let out = tauri::async_runtime::spawn_blocking(move || rec.finish())
        .await
        .map_err(|e| format!("结束录像失败：{e}"))?;
    match out {
        Ok((name, path, secs, thumb)) => {
            if let Some(t) = &thumb {
                app.viewer.put_thumb(&name, t.jpeg.clone());
            }
            Ok(RecordStop {
                thumb_url: thumb.map(|_| app.viewer.url(&format!("/thumb/{name}"))),
                name,
                path: path.to_string_lossy().to_string(),
                secs,
                error: None,
            })
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
fn recording_status(app: State<'_, Arc<App>>) -> RecordStatus {
    let core = match app.core.lock() {
        Ok(c) => c,
        Err(_) => {
            return RecordStatus { recording: false, secs: 0.0, name: None };
        }
    };
    match &core.recorder {
        Some(r) => RecordStatus { recording: true, secs: r.elapsed(), name: Some(r.name.clone()) },
        None => RecordStatus { recording: false, secs: 0.0, name: None },
    }
}

#[tauri::command]
fn pair_info(app: State<'_, Arc<App>>, preferred: Option<String>) -> PairResult {
    // 每次调用都重新读取本机 IP：换 Wi‑Fi / 插网线 / 关 VPN 后二维码地址随之更新。
    let info = app.wireless.refresh(preferred.as_deref());
    if info.available && !info.qr.is_empty() {
        app.viewer.set_qr(info.qr.clone());
    }
    PairResult {
        url: info.url,
        qr_url: app.viewer.url("/qr"),
        lan: info.lan,
        ips: info.ips,
        port: info.port,
        token: info.token,
        available: info.available,
        hint: info.hint,
    }
}

/// 无线配对的实时诊断：手机连没连上、页面开没开、画面推没推过来。
#[tauri::command]
fn pair_diag(app: State<'_, Arc<App>>) -> serde_json::Value {
    app.wireless.diag()
}

/// 自检专用：让 WebView 里的 JS 把探测结果回传到 stdout。
#[tauri::command]
fn probe_report(tag: String, ok: bool, detail: String) {
    println!("PROBE_JS tag={tag} ok={ok} detail={detail}");
}

#[tauri::command]
fn pair_stop(app: State<'_, Arc<App>>) -> serde_json::Value {
    app.wireless.stop();
    serde_json::json!({ "ok": true })
}

#[tauri::command]
fn captures_dir() -> CapturesDir {
    CapturesDir { dir: capture::captures_dir().to_string_lossy().to_string() }
}

#[tauri::command]
fn open_captures_dir() -> Result<serde_json::Value, String> {
    let dir = capture::captures_dir();
    capture::ensure_dir(&dir)?;
    open_target(&dir, false)?;
    Ok(serde_json::json!({ "ok": true, "dir": dir.to_string_lossy() }))
}

#[tauri::command]
fn open_path(path: String, reveal: bool) -> Result<serde_json::Value, String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("文件不存在".to_string());
    }
    open_target(&p, reveal)?;
    Ok(serde_json::json!({ "ok": true }))
}

fn open_target(path: &std::path::Path, reveal: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        if reveal {
            c.arg(format!("/select,{}", path.display()));
        } else {
            c.arg(path);
        }
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        if reveal {
            c.arg("-R");
        }
        c.arg(path);
        c
    };
    cmd.status().map_err(|e| format!("无法打开：{e}"))?;
    Ok(())
}

#[tauri::command]
fn delete_capture(app: State<'_, Arc<App>>, name: String) -> Result<serde_json::Value, String> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return Err("文件名无效".to_string());
    }
    let path = capture::captures_dir().join(&name);
    std::fs::remove_file(&path).map_err(|e| format!("删除失败：{e}"))?;
    app.viewer.forget_thumb(&name);
    Ok(serde_json::json!({ "ok": true }))
}

fn state_payload(gen: u64, state: &SourceState) -> SourceStatePayload {
    let (label, detail) = match state {
        SourceState::Starting => ("starting", None),
        SourceState::Live => ("live", None),
        SourceState::Error(e) => ("error", Some(e.clone())),
        SourceState::Stopped => ("stopped", None),
    };
    SourceStatePayload { gen, state: label.to_string(), detail }
}

fn spawn_pump(app: Arc<App>, handle: AppHandle, rx: Receiver<Event>) {
    std::thread::spawn(move || {
        let mut last_tick = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Event::Frame { gen, frame }) => {
                    let mut core = match app.core.lock() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if gen != core.gen {
                        continue;
                    }
                    app.viewer.set_frame(frame.jpeg.clone());
                    if let Some(rec) = &core.recorder {
                        rec.push(frame.clone());
                    }
                    core.fps_frames += 1;
                    let elapsed = core.fps_mark.elapsed();
                    let mut info = None;
                    if elapsed >= Duration::from_millis(900) {
                        let fps = core.fps_frames as f32 / elapsed.as_secs_f32();
                        core.fps_value = fps;
                        core.fps_frames = 0;
                        core.fps_mark = Instant::now();
                        info = Some(FrameInfoPayload { gen, width: frame.width, height: frame.height, fps });
                    }
                    core.frame = Some(frame);
                    drop(core);
                    if let Some(i) = info {
                        let _ = handle.emit("frame-info", i);
                    }
                }
                Ok(Event::State { gen, state }) => {
                    if let Ok(core) = app.core.lock() {
                        if gen != core.gen {
                            continue;
                        }
                    }
                    if matches!(state, SourceState::Error(_) | SourceState::Stopped) {
                        app.viewer.clear();
                        if let Ok(mut core) = app.core.lock() {
                            core.frame = None;
                        }
                    }
                    let payload = state_payload(gen, &state);
                    let _ = handle.emit("source-state", payload);
                    if let SourceState::Error(msg) = &state {
                        let _ = handle.emit("notice", NoticePayload { text: msg.clone(), level: "error".into() });
                    }
                }
                Ok(Event::Wireless { connected, name, hint }) => {
                    let _ = handle.emit("wireless", WirelessPayload { connected, name, hint });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                if let Ok(core) = app.core.lock() {
                    if let Some(rec) = &core.recorder {
                        let _ = handle.emit("record-tick", RecordTickPayload { secs: rec.elapsed() });
                    }
                }
            }
        }
    });
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let dir = capture::captures_dir();
            let _ = capture::ensure_dir(&dir);
            let viewer = Viewer::start(dir).map_err(|e| format!("启动本地取景服务失败：{e}"))?;
            #[cfg(debug_assertions)]
            eprintln!("[monitor] 取景服务已启动 {}", viewer.base());
            let (tx, rx) = channel();
            let wireless = Wireless::new(tx.clone());
            let state = Arc::new(App { viewer, wireless, tx, core: Mutex::new(Core {
                gen: 0,
                current: None,
                source: None,
                frame: None,
                recorder: None,
                fps_mark: Instant::now(),
                fps_frames: 0,
                fps_value: 0.0,
            }) });
            app.manage(state.clone());
            spawn_pump(state.clone(), app.handle().clone(), rx);
            // 无界面自检：MONITOR_PROBE=1 时直接开无线配对服务并启动无线取景源，
            // 把配对地址打到 stdout，方便用脚本/浏览器验证整条链路（不影响正常使用）。
            if std::env::var("MONITOR_PROBE").is_ok() {
                let info = state.wireless.ensure_started();
                if !info.qr.is_empty() {
                    state.viewer.set_qr(info.qr.clone());
                }
                println!("PROBE_URL={}", info.url);
                println!("PROBE_TOKEN={}", info.token);
                println!("PROBE_STREAM={}", state.viewer.url("/stream"));
                println!("PROBE_QR={}", state.viewer.url("/qr"));
                println!("PROBE_AVAILABLE={}", info.available);
                let gen = {
                    let mut core = match state.core.lock() { Ok(c) => c, Err(_) => return Ok(()) };
                    core.gen += 1;
                    core.current = Some(DeviceId::Wireless);
                    core.gen
                };
                match source::spawn_wireless(gen, state.tx.clone(), state.wireless.clone()) {
                    Ok(handle) => {
                        if let Ok(mut core) = state.core.lock() {
                            if core.gen == gen { core.source = Some(handle); }
                        }
                    }
                    Err(err) => println!("PROBE_ERR={err}"),
                }
                // 在真实 WebView 里验证本地取景服务能不能被 fetch 到（CORS / 流式读取）。
                let stream_url = state.viewer.url("/stream");
                let health_url = state.viewer.url("/health");
                let frame_url = state.viewer.url("/frame.jpg");
                let js = r#"(function(){
  function send(tag, ok, detail){
    try { window.__TAURI__.core.invoke('probe_report', { tag: tag, ok: ok, detail: String(detail) }); } catch (e) {}
  }
  send('boot', true, location.protocol + '//' + location.host);
  fetch('@HEALTH@', { cache: 'no-store' })
    .then(function(r){ send('health', r.ok, 'status=' + r.status); })
    .catch(function(e){ send('health', false, e.name + ': ' + e.message); });
  fetch('@STREAM@', { cache: 'no-store' })
    .then(function(r){
      send('stream_open', r.ok, 'status=' + r.status + ' body=' + (!!r.body));
      if (!r.body) return;
      var rd = r.body.getReader(), chunks = 0, bytes = 0, t0 = Date.now();
      (function loop(){
        rd.read().then(function(x){
          if (x.done) { send('stream_end', true, 'chunks=' + chunks + ' bytes=' + bytes); return; }
          chunks++; bytes += x.value.length;
          if (Date.now() - t0 > 6000) {
            try { rd.cancel(); } catch (e) {}
            send('stream_read', true, 'chunks=' + chunks + ' bytes=' + bytes + ' ms=' + (Date.now() - t0));
            return;
          }
          loop();
        }).catch(function(e){
          send('stream_read', false, 'chunks=' + chunks + ' bytes=' + bytes + ' ' + e.name + ': ' + e.message);
        });
      })();
    })
    .catch(function(e){ send('stream_open', false, e.name + ': ' + e.message); });
  setTimeout(function(){
    fetch('@FRAME@', { cache: 'no-store' })
      .then(function(r){ return r.arrayBuffer().then(function(b){ send('frame', r.ok, 'status=' + r.status + ' bytes=' + b.byteLength); }); })
      .catch(function(e){ send('frame', false, e.name + ': ' + e.message); });
  }, 2500);
  setTimeout(function(){
    var se = document.getElementById('stage-empty');
    var fps = document.getElementById('hud-fps');
    var st = document.getElementById('status-text');
    send('ui', !!(se && se.hidden), 'stageEmptyHidden=' + (!!(se && se.hidden)) + ' fps=' + (fps ? fps.textContent : '?') + ' status=' + (st ? st.textContent : '?'));
  }, 24000);
  // 放大取景：点一下按钮，确认真实 WKWebView 里布局铺满、并且窗口进了系统全屏。
  setTimeout(function(){
    var btn = document.getElementById('btn-zoom');
    if (!btn) { send('zoom', false, 'no #btn-zoom'); return; }
    btn.click();
    setTimeout(function(){
      var app = document.getElementById('app');
      var r = document.getElementById('stage').getBoundingClientRect();
      var on = !!(app && app.classList.contains('is-immersive'));
      var api = window.__TAURI__ && window.__TAURI__.window;
      var full = api && api.getCurrentWindow ? api.getCurrentWindow().isFullscreen() : 'n/a';
      Promise.resolve(full).then(function(f){
        send('zoom', on && Math.abs(r.width - innerWidth) < 2, 'immersive=' + on + ' stage=' + Math.round(r.width) + 'x' + Math.round(r.height) + ' win=' + innerWidth + 'x' + innerHeight + ' fullscreen=' + f);
        btn.click();
      }).catch(function(e){ send('zoom', false, 'err ' + e); });
    }, 1500);
  }, 5000);
  // 轮询通道：连续取 40 次单帧，统计成功率与解码成功率（模拟新的桌面端取帧方式）。
  setTimeout(function(){
    var ok = 0, miss = 0, decoded = 0, fail = 0, firstErr = '', t0 = Date.now();
    var url = '@FRAME@';
    (function poll(n){
      if (n <= 0) {
        send('poll', fail === 0 && decoded > 0, 'ok=' + ok + ' 404=' + miss + ' decoded=' + decoded + ' fail=' + fail + ' ms=' + (Date.now() - t0) + ' ' + firstErr);
        return;
      }
      fetch(url + '?t=' + Date.now(), { cache: 'no-store' })
        .then(function(r){
          if (r.status === 404) { miss++; return null; }
          if (!r.ok) { fail++; firstErr = 'status ' + r.status; return null; }
          return r.arrayBuffer().then(function(b){
            ok++;
            return window.createImageBitmap(new Blob([b], { type: 'image/jpeg' })).then(function(bm){ decoded++; bm.close(); }).catch(function(e){ firstErr = 'decode ' + e.name; });
          });
        })
        .catch(function(e){ fail++; if (!firstErr) firstErr = e.name + ': ' + e.message; })
        .then(function(){ setTimeout(function(){ poll(n - 1); }, 40); });
    })(40);
  }, 3000);
})();"#
                    .replace("@HEALTH@", &health_url)
                    .replace("@STREAM@", &stream_url)
                    .replace("@FRAME@", &frame_url);
                let win = app.get_webview_window("main");
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    match win {
                        Some(w) => {
                            println!("PROBE_JS_RUN={}", js.len());
                            if let Err(err) = w.eval(&js) {
                                println!("PROBE_JS_ERR={err}");
                            }
                        }
                        None => println!("PROBE_JS_ERR=no main window"),
                    }
                });
                // 让真实界面自己走一遍：模拟手机连上来 → 界面选源 → 拉流。
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(12));
                    let _ = handle.emit("wireless", serde_json::json!({ "connected": true, "name": "probe", "hint": "" }));
                    println!("PROBE_EMIT=wireless");
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_devices,
            start_source,
            stop_source,
            take_photo,
            start_recording,
            stop_recording,
            recording_status,
            pair_info,
            pair_diag,
            probe_report,
            pair_stop,
            open_path,
            open_captures_dir,
            delete_capture,
            captures_dir
        ])
        .run(tauri::generate_context!())
        .expect("MONITOR 启动失败");
}

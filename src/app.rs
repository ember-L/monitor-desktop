//! MONITOR 桌面取景器 UI：设备切换、比例布局、取景叠加、拍照录像、胶片条。

use crate::capture::{self, Recorder};
use crate::ratio::{self, Mode};
use crate::source::{self, DeviceId, DeviceLists, Event, Frame, SourceHandle, SourceState, Thumb};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

struct FilmItem {
    path: PathBuf,
    is_video: bool,
    thumb: egui::TextureHandle,
    full: Option<egui::TextureHandle>,
    label: String,
    secs: f64,
}

pub struct MonitorApp {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    devices: DeviceLists,
    enumerating: bool,
    last_enum: Instant,
    source: Option<SourceHandle>,
    source_gen: u64,
    current: Option<DeviceId>,
    state: SourceState,
    frame: Option<Frame>,
    tex: Option<egui::TextureHandle>,
    fps: f32,
    ratio_sel: usize,
    custom_w: String,
    custom_h: String,
    mode: Mode,
    grid: bool,
    mirror: bool,
    fullscreen: bool,
    recorder: Option<Recorder>,
    film: Vec<FilmItem>,
    viewer: Option<usize>,
    notice: Option<(String, Instant)>,
    flash: f32,
    dir: PathBuf,
}

impl MonitorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 中文字体（macOS 系统字体，TTC 提取单字体）
        if let Some(bytes) = crate::font::load_cjk_font() {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&family) {
                    list.insert(0, "cjk".to_owned());
                }
            }
            cc.egui_ctx.set_fonts(fonts);
        }
        let (tx, rx) = channel();
        let dir = capture::captures_dir();
        let mut app = Self {
            tx: tx.clone(),
            rx,
            devices: DeviceLists::default(),
            enumerating: false,
            last_enum: Instant::now() - Duration::from_secs(60),
            source: None,
            source_gen: 0,
            current: None,
            state: SourceState::Stopped,
            frame: None,
            tex: None,
            fps: 0.0,
            ratio_sel: 0,
            custom_w: "16".into(),
            custom_h: "9".into(),
            mode: Mode::Fit,
            grid: false,
            mirror: false,
            fullscreen: false,
            recorder: None,
            film: Vec::new(),
            viewer: None,
            notice: None,
            flash: 0.0,
            dir,
        };
        app.enumerate();
        app
    }

    fn enumerate(&mut self) {
        if self.enumerating {
            return;
        }
        self.enumerating = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || source::enumerate_devices(tx));
    }

    fn start_source(&mut self, id: DeviceId) {
        if let Some(mut rec) = self.recorder.take() {
            rec.stop();
        }
        self.source = None; // Drop 旧源
        self.source_gen += 1;
        let gen = self.source_gen;
        self.current = Some(id.clone());
        self.frame = None;
        self.tex = None;
        self.fps = 0.0;
        self.state = SourceState::Starting;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match id {
                DeviceId::Av(i) => source::spawn_camera(gen, i, tx.clone()),
                DeviceId::Android(s) => source::spawn_android(gen, &s, tx.clone()),
            };
            let _ = tx.send(Event::SourceReady { gen, result });
        });
    }

    fn current_ratio(&self) -> Option<(f32, f32)> {
        if self.ratio_sel < ratio::PRESETS.len() {
            return ratio::PRESETS[self.ratio_sel].1;
        }
        let w: f32 = self.custom_w.trim().parse().ok()?;
        let h: f32 = self.custom_h.trim().parse().ok()?;
        if w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite() {
            Some((w, h))
        } else {
            None
        }
    }

    fn ratio_label(&self) -> String {
        if self.ratio_sel < ratio::PRESETS.len() {
            ratio::PRESETS[self.ratio_sel].0.to_string()
        } else {
            format!("自定义 {}:{}", self.custom_w.trim(), self.custom_h.trim())
        }
    }

    fn take_photo(&mut self) {
        let Some(frame) = &self.frame else {
            self.notice = Some(("没有可拍摄的画面".into(), Instant::now()));
            return;
        };
        capture::save_photo(frame, self.mirror, &self.dir, self.tx.clone());
        self.flash = 1.0;
    }

    fn toggle_record(&mut self) {
        if let Some(mut rec) = self.recorder.take() {
            rec.stop();
            self.notice = Some(("正在收尾录像文件…".into(), Instant::now()));
            return;
        }
        let Some(frame) = &self.frame else {
            self.notice = Some(("没有可录制的画面".into(), Instant::now()));
            return;
        };
        match capture::start_recording(frame, self.fps, &self.dir, self.tx.clone()) {
            Ok(rec) => {
                self.notice = Some((format!("开始录像：{}", rec.path.file_name().unwrap_or_default().to_string_lossy()), Instant::now()));
                self.recorder = Some(rec);
            }
            Err(e) => self.notice = Some((e, Instant::now())),
        }
    }

    fn add_film(&mut self, ctx: &egui::Context, path: PathBuf, thumb: Thumb, is_video: bool, secs: f64) {
        let img = egui::ColorImage::from_rgb([thumb.width as usize, thumb.height as usize], &thumb.rgb);
        let tex = ctx.load_texture(format!("film{}", self.film.len()), img, egui::TextureOptions::LINEAR);
        let label = chrono::Local::now().format("%H:%M:%S").to_string();
        self.film.push(FilmItem { path, is_video, thumb: tex, full: None, label, secs });
    }

    fn open_path(path: &std::path::Path, reveal: bool) {
        let mut cmd = std::process::Command::new("open");
        if reveal {
            cmd.arg("-R");
        }
        cmd.arg(path);
        let _ = cmd.spawn();
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        let mut latest: Option<Frame> = None;
        for _ in 0..512 {
            let Ok(ev) = self.rx.try_recv() else { break };
            match ev {
                Event::Frame { gen, frame } if gen == self.source_gen => latest = Some(frame),
                Event::State { gen, state } if gen == self.source_gen => self.state = state,
                Event::Fps { gen, fps } if gen == self.source_gen => self.fps = fps,
                Event::SourceReady { gen, result } if gen == self.source_gen => match result {
                    Ok(handle) => self.source = Some(handle),
                    Err(e) => self.state = SourceState::Error(e),
                },
                Event::Devices(list) => {
                    self.devices = list;
                    self.enumerating = false;
                    self.last_enum = Instant::now();
                }
                Event::PhotoSaved { path, thumb } => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    self.add_film(ctx, path, thumb, false, 0.0);
                    self.notice = Some((format!("已保存 {name}"), Instant::now()));
                }
                Event::RecordingDone { path, thumb, secs, error } => {
                    self.recorder = None;
                    match error {
                        Some(e) => self.notice = Some((format!("录像失败：{e}"), Instant::now())),
                        None => {
                            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                            if let Some(thumb) = thumb {
                                self.add_film(ctx, path, thumb, true, secs);
                            }
                            self.notice = Some((format!("录像已保存 {name}（{secs:.1}s）"), Instant::now()));
                        }
                    }
                }
                Event::Notice(msg) => self.notice = Some((msg, Instant::now())),
                _ => {}
            }
        }
        if let Some(frame) = latest {
            let img = egui::ColorImage::from_rgb([frame.width as usize, frame.height as usize], &frame.rgb);
            match &mut self.tex {
                Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                None => self.tex = Some(ctx.load_texture("viewfinder", img, egui::TextureOptions::LINEAR)),
            }
            if let Some(rec) = &self.recorder {
                rec.push(&frame);
            }
            self.frame = Some(frame);
        }
    }

    fn device_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("设备");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    self.enumerate();
                }
            });
        });
        if self.enumerating {
            ui.spinner();
        }
        if !self.devices.ffmpeg_ok {
            ui.colored_label(egui::Color32::YELLOW, "未找到 ffmpeg：brew install ffmpeg");
        }
        ui.add_space(4.0);

        let ios: Vec<_> = self.devices.av.iter().filter(|d| d.name.contains("iPhone") || d.name.contains("iPad")).cloned().collect();
        let cams: Vec<_> = self.devices.av.iter().filter(|d| !(d.name.contains("iPhone") || d.name.contains("iPad"))).cloned().collect();

        ui.strong("本机摄像头");
        if cams.is_empty() {
            ui.weak("未发现");
        }
        for d in &cams {
            let selected = self.current == Some(DeviceId::Av(d.index));
            if ui.selectable_label(selected, &d.name).clicked() {
                self.start_source(DeviceId::Av(d.index));
            }
        }
        ui.add_space(4.0);
        ui.strong("iPhone / iPad（USB）");
        if ios.is_empty() {
            ui.weak("未发现（USB 连接并信任后出现）");
        }
        for d in &ios {
            let selected = self.current == Some(DeviceId::Av(d.index));
            if ui.selectable_label(selected, format!("📱 {}", d.name)).clicked() {
                self.start_source(DeviceId::Av(d.index));
            }
        }
        ui.add_space(4.0);
        ui.strong("Android（adb）");
        if !self.devices.adb_ok {
            ui.weak("未找到 adb");
        } else if self.devices.android.is_empty() {
            ui.weak("未发现（需开启 USB 调试）");
        }
        for d in &self.devices.android.clone() {
            let selected = self.current == Some(DeviceId::Android(d.serial.clone()));
            if ui.selectable_label(selected, format!("🤖 {}", d.model)).on_hover_text(&d.serial).clicked() {
                self.start_source(DeviceId::Android(d.serial.clone()));
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.heading("屏幕比例");
        let label = self.ratio_label();
        egui::ComboBox::from_id_salt("ratio")
            .selected_text(label)
            .show_ui(ui, |ui| {
                for (i, (name, _)) in ratio::PRESETS.iter().enumerate() {
                    ui.selectable_value(&mut self.ratio_sel, i, *name);
                }
                ui.selectable_value(&mut self.ratio_sel, ratio::PRESETS.len(), "自定义…");
            });
        if self.ratio_sel >= ratio::PRESETS.len() {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.custom_w).desired_width(44.0));
                ui.label(":");
                ui.add(egui::TextEdit::singleline(&mut self.custom_h).desired_width(44.0));
            });
            if self.current_ratio().is_none() {
                ui.colored_label(egui::Color32::YELLOW, "请输入正数宽高");
            }
        }
        ui.horizontal(|ui| {
            for m in Mode::ALL {
                ui.radio_value(&mut self.mode, m, m.label()).on_hover_text(m.hint());
            }
        });

        ui.add_space(10.0);
        ui.separator();
        ui.heading("取景");
        ui.checkbox(&mut self.grid, "网格（G）");
        ui.checkbox(&mut self.mirror, "镜像（M）");
        ui.add_space(6.0);
        let live = self.frame.is_some();
        ui.add_enabled_ui(live, |ui| {
            if ui.button("📷 拍照（空格）").clicked() {
                self.take_photo();
            }
            let rec = self.recorder.is_some();
            let text = if rec { "⏹ 停止录像（R）" } else { "⏺ 录像（R）" };
            let btn = egui::Button::new(egui::RichText::new(text).color(if rec { egui::Color32::WHITE } else { ui.visuals().text_color() }))
                .fill(if rec { egui::Color32::from_rgb(180, 40, 40) } else { ui.visuals().widgets.inactive.bg_fill });
            if ui.add(btn).clicked() {
                self.toggle_record();
            }
        });
        if ui.button("📂 打开保存目录").clicked() {
            let _ = std::fs::create_dir_all(&self.dir);
            Self::open_path(&self.dir, false);
        }
        ui.add_space(8.0);
        ui.weak("保存位置：captures/");
    }

    fn viewfinder(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(14));
        if let (Some(frame), Some(tex)) = (&self.frame, &self.tex) {
            let l = ratio::layout(frame.width, frame.height, rect, self.current_ratio(), self.mode);
            let uv = if self.mirror {
                egui::Rect::from_min_max(
                    egui::pos2(1.0 - l.uv.max.x, l.uv.min.y),
                    egui::pos2(1.0 - l.uv.min.x, l.uv.max.y),
                )
            } else {
                l.uv
            };
            painter.image(tex.id(), l.dest, uv, egui::Color32::WHITE);
            if self.grid {
                let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(80));
                for i in 1..3 {
                    let x = l.dest.left() + l.dest.width() * i as f32 / 3.0;
                    let y = l.dest.top() + l.dest.height() * i as f32 / 3.0;
                    painter.line_segment([egui::pos2(x, l.dest.top()), egui::pos2(x, l.dest.bottom())], stroke);
                    painter.line_segment([egui::pos2(l.dest.left(), y), egui::pos2(l.dest.right(), y)], stroke);
                }
            }
            // 叠加信息：时间码 / 分辨率 / FPS
            let now = chrono::Local::now().format("%H:%M:%S");
            let info = format!("{now}  {}×{}  {:.0} FPS", frame.width, frame.height, self.fps);
            let galley = painter.layout_no_wrap(info, egui::FontId::monospace(13.0), egui::Color32::WHITE);
            let pos = l.dest.left_top() + egui::vec2(8.0, 6.0);
            painter.rect_filled(egui::Rect::from_min_size(pos - egui::vec2(4.0, 2.0), galley.size() + egui::vec2(8.0, 4.0)), 4.0, egui::Color32::from_black_alpha(120));
            painter.galley(pos, galley, egui::Color32::WHITE);
            if self.recorder.is_some() {
                let secs = self.recorder.as_ref().map(|r| r.elapsed_secs()).unwrap_or(0.0);
                let text = format!("⏺ REC {:02}:{:02}", secs as u64 / 60, secs as u64 % 60);
                let galley = painter.layout_no_wrap(text, egui::FontId::monospace(14.0), egui::Color32::from_rgb(255, 80, 80));
                let pos = egui::pos2(l.dest.right() - galley.size().x - 10.0, l.dest.top() + 6.0);
                painter.rect_filled(egui::Rect::from_min_size(pos - egui::vec2(4.0, 2.0), galley.size() + egui::vec2(8.0, 4.0)), 4.0, egui::Color32::from_black_alpha(120));
                painter.galley(pos, galley, egui::Color32::WHITE);
            }
        } else {
            let msg = match &self.state {
                SourceState::Starting => "正在连接设备…".to_string(),
                SourceState::Error(e) => format!("⚠ {e}"),
                _ => "在左侧选择设备开始取景".to_string(),
            };
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(18.0), egui::Color32::from_gray(160));
        }
        if self.flash > 0.0 {
            painter.rect_filled(rect, 0.0, egui::Color32::from_white_alpha((self.flash * 140.0) as u8));
            self.flash = (self.flash - 0.08).max(0.0);
            ctx.request_repaint();
        }
    }

    fn filmstrip(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.strong("胶片");
            ui.weak(format!("{} 项", self.film.len()));
            if let Some((msg, t)) = &self.notice {
                if t.elapsed() < Duration::from_secs(5) {
                    ui.weak(msg);
                }
            }
        });
        let mut open_viewer: Option<usize> = None;
        egui::ScrollArea::horizontal().id_salt("film").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.film.is_empty() {
                    ui.weak("拍照 / 录像后出现在这里");
                }
                for (i, item) in self.film.iter().enumerate() {
                    let h = 72.0;
                    let [tw, th] = item.thumb.size();
                    let w = h * tw as f32 / th as f32;
                    ui.vertical(|ui| {
                        let img = egui::load::SizedTexture::new(item.thumb.id(), egui::vec2(w, h));
                        if ui.add(egui::ImageButton::new(img)).clicked() {
                            open_viewer = Some(i);
                        }
                        let tag = if item.is_video { format!("▶ {} {:.0}s", item.label, item.secs) } else { format!("🖼 {}", item.label) };
                        ui.weak(tag);
                    });
                }
            });
        });
        if let Some(i) = open_viewer {
            // 照片按需加载原图
            if !self.film[i].is_video && self.film[i].full.is_none() {
                if let Ok(img) = image::open(&self.film[i].path) {
                    let rgb = img.to_rgb8();
                    let (w, h) = rgb.dimensions();
                    let ci = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
                    self.film[i].full = Some(ctx.load_texture(format!("full{i}"), ci, egui::TextureOptions::LINEAR));
                }
            }
            self.viewer = Some(i);
        }
    }

    fn viewer_window(&mut self, ctx: &egui::Context) {
        let Some(i) = self.viewer else { return };
        if i >= self.film.len() {
            self.viewer = None;
            return;
        }
        let mut open = true;
        let mut delete = false;
        let mut reveal = false;
        let mut play = false;
        egui::Window::new("查看")
            .open(&mut open)
            .default_size([760.0, 600.0])
            .show(ctx, |ui| {
                let item = &self.film[i];
                ui.label(item.path.to_string_lossy().to_string());
                if let Some(tex) = &item.full {
                    ui.add(egui::Image::from_texture(tex).max_size(ui.available_size() - egui::vec2(0.0, 40.0)));
                } else {
                    ui.add(egui::Image::from_texture(&item.thumb).max_size(ui.available_size() - egui::vec2(0.0, 40.0)));
                    if item.is_video {
                        ui.weak(format!("视频（{:.1}s）——点「打开」用系统播放器播放", item.secs));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("打开").clicked() {
                        play = true;
                    }
                    if ui.button("在 Finder 中显示").clicked() {
                        reveal = true;
                    }
                    if ui.button("🗑 删除").clicked() {
                        delete = true;
                    }
                });
            });
        if play {
            Self::open_path(&self.film[i].path, false);
        }
        if reveal {
            Self::open_path(&self.film[i].path, true);
        }
        if delete {
            let _ = std::fs::remove_file(&self.film[i].path);
            self.film.remove(i);
            self.viewer = None;
        } else if !open {
            self.viewer = None;
        }
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        if self.last_enum.elapsed() > Duration::from_secs(10) {
            self.enumerate();
        }
        // 快捷键
        if !ctx.wants_keyboard_input() {
            let (space, g, m, r, f) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::Space),
                    i.key_pressed(egui::Key::G),
                    i.key_pressed(egui::Key::M),
                    i.key_pressed(egui::Key::R),
                    i.key_pressed(egui::Key::F),
                )
            });
            if space {
                self.take_photo();
            }
            if g {
                self.grid = !self.grid;
            }
            if m {
                self.mirror = !self.mirror;
            }
            if r {
                self.toggle_record();
            }
            if f {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.strong("MONITOR");
                ui.separator();
                let (state_text, color) = match &self.state {
                    SourceState::Starting => ("连接中…", egui::Color32::YELLOW),
                    SourceState::Live => ("实时", egui::Color32::from_rgb(80, 200, 120)),
                    SourceState::Error(_) => ("错误", egui::Color32::from_rgb(230, 90, 90)),
                    SourceState::Stopped => ("未连接", egui::Color32::GRAY),
                };
                ui.colored_label(color, state_text);
                if let Some(cur) = &self.current {
                    let name = match cur {
                        DeviceId::Av(i) => self.devices.av.iter().find(|d| d.index == *i).map(|d| d.name.clone()).unwrap_or_else(|| format!("设备 {i}")),
                        DeviceId::Android(s) => self.devices.android.iter().find(|d| &d.serial == s).map(|d| d.model.clone()).unwrap_or_else(|| s.clone()),
                    };
                    ui.label(name);
                }
                if let Some(rec) = &self.recorder {
                    let secs = rec.elapsed_secs();
                    ui.colored_label(egui::Color32::from_rgb(255, 80, 80), format!("⏺ {:02}:{:02}", secs as u64 / 60, secs as u64 % 60));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let fs_text = if self.fullscreen { "退出全屏（F）" } else { "全屏（F）" };
                    if ui.button(fs_text).clicked() {
                        self.fullscreen = !self.fullscreen;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::SidePanel::left("panel").exact_width(252.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.device_panel(ui));
        });

        egui::TopBottomPanel::bottom("film").exact_height(128.0).show(ctx, |ui| {
            ui.add_space(4.0);
            self.filmstrip(ui, ctx);
        });

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            self.viewfinder(ui, ctx);
        });

        self.viewer_window(ctx);

        if matches!(self.state, SourceState::Live) || self.recorder.is_some() || self.flash > 0.0 {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        if let Some(mut rec) = self.recorder.take() {
            rec.stop();
        }
        if let Some(mut src) = self.source.take() {
            src.stop();
        }
    }
}

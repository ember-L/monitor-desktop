//! 屏幕比例布局：把任意源画面放进指定比例的显示框。
//! Fit = 完整显示（框内留黑边）；Fill = 裁切填满；Stretch = 拉伸填满。

use egui::{pos2, vec2, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Fit,
    Fill,
    Stretch,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Fit, Mode::Fill, Mode::Stretch];
    pub fn label(self) -> &'static str {
        match self {
            Mode::Fit => "适应",
            Mode::Fill => "裁切",
            Mode::Stretch => "拉伸",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Mode::Fit => "完整画面，比例框内留黑边",
            Mode::Fill => "画面裁切填满比例框",
            Mode::Stretch => "画面拉伸变形填满比例框",
        }
    }
}

/// (显示名, 宽高比)；None = 跟随源画面比例。最后一项之外另有「自定义」。
pub const PRESETS: &[(&str, Option<(f32, f32)>)] = &[
    ("跟随源", None),
    ("16:9", Some((16.0, 9.0))),
    ("9:16", Some((9.0, 16.0))),
    ("4:3", Some((4.0, 3.0))),
    ("3:4", Some((3.0, 4.0))),
    ("3:2", Some((3.0, 2.0))),
    ("2:3", Some((2.0, 3.0))),
    ("21:9", Some((21.0, 9.0))),
    ("1:1", Some((1.0, 1.0))),
];

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// 实际绘制到屏幕上的矩形
    pub dest: Rect,
    /// 采样源画面的 UV 区域（Fill 模式下为裁切区域）
    pub uv: Rect,
}

pub fn layout(src_w: u32, src_h: u32, avail: Rect, ratio: Option<(f32, f32)>, mode: Mode) -> Layout {
    let sw = src_w.max(1) as f32;
    let sh = src_h.max(1) as f32;
    let target = match ratio {
        Some((a, b)) if a > 0.0 && b > 0.0 && a.is_finite() && b.is_finite() => a / b,
        _ => sw / sh,
    };
    // 比例框：avail 内能放下的最大 target 比例矩形
    let (bw, bh) = if avail.width() / avail.height().max(0.001) > target {
        (avail.height() * target, avail.height())
    } else {
        (avail.width(), avail.width() / target)
    };
    let box_rect = Rect::from_center_size(avail.center(), vec2(bw.max(1.0), bh.max(1.0)));
    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    match mode {
        Mode::Fit => {
            let scale = (bw / sw).min(bh / sh);
            let dest = Rect::from_center_size(avail.center(), vec2(sw * scale, sh * scale));
            Layout { dest, uv: full_uv }
        }
        Mode::Fill => {
            let scale = (bw / sw).max(bh / sh);
            let vis_w = (bw / scale).min(sw);
            let vis_h = (bh / scale).min(sh);
            let u0 = (sw - vis_w) / 2.0 / sw;
            let v0 = (sh - vis_h) / 2.0 / sh;
            Layout {
                dest: box_rect,
                uv: Rect::from_min_max(pos2(u0, v0), pos2(u0 + vis_w / sw, v0 + vis_h / sh)),
            }
        }
        Mode::Stretch => Layout { dest: box_rect, uv: full_uv },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))
    }

    #[test]
    fn auto_fit_is_contain() {
        let l = layout(1920, 1080, avail(), None, Mode::Fit);
        assert!((l.dest.width() - 800.0).abs() < 0.5);
        assert!((l.dest.height() - 450.0).abs() < 0.5);
        assert_eq!(l.uv, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
    }

    #[test]
    fn preset_box_has_target_ratio() {
        let l = layout(1080, 2400, avail(), Some((16.0, 9.0)), Mode::Fill);
        assert!((l.dest.width() / l.dest.height() - 16.0 / 9.0).abs() < 0.01);
        // 竖屏源填 16:9 框：上下裁切，UV 高度小于 1
        assert!(l.uv.height() < 0.5);
        assert!((l.uv.width() - 1.0).abs() < 0.001);
    }

    #[test]
    fn stretch_fills_box() {
        let l = layout(640, 480, avail(), Some((1.0, 1.0)), Mode::Stretch);
        assert!((l.dest.width() - 600.0).abs() < 0.5);
        assert!((l.dest.height() - 600.0).abs() < 0.5);
        assert_eq!(l.uv.width(), 1.0);
    }

    #[test]
    fn fit_inside_preset_box_letterboxes() {
        // 4:3 源放进 21:9 框：左右留黑边，画面完整
        let l = layout(640, 480, avail(), Some((21.0, 9.0)), Mode::Fit);
        assert!(l.dest.width() < l.dest.height() * 2.0);
        assert_eq!(l.uv.height(), 1.0);
    }

    #[test]
    fn custom_ratio_respected() {
        let l = layout(1920, 1080, avail(), Some((2.0, 1.0)), Mode::Fill);
        assert!((l.dest.width() / l.dest.height() - 2.0).abs() < 0.01);
    }
}

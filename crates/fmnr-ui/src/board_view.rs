//! 白板画布：坐标变换与节点布局。
//!
//! 画布坐标（[`fmnr_core::board::Pos`]）是**用户数据**，屏幕坐标是渲染细节。
//! 两者之间的变换集中在这里，并且是纯函数——拖动节点时「画布坐标 +=
//! 屏幕位移 / 缩放」这条换算一旦写错，用户会看到节点跟不上鼠标，
//! 而那种 bug 在 UI 里很难复现。所以它必须能被单测钉住。

use fmnr_core::board::Pos;

/// 白板相机：平移（屏幕像素）+ 缩放。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// 画布原点在屏幕上的位置。
    pub pan: egui::Vec2,
    pub zoom: f32,
}

/// 缩放下限/上限。下限防止缩到看不见，上限防止放大到像素糊掉。
pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 3.0;

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: egui::Vec2::new(40.0, 40.0),
            zoom: 1.0,
        }
    }
}

impl Camera {
    /// 画布坐标 → 屏幕坐标。
    pub fn to_screen(&self, p: Pos, origin: egui::Pos2) -> egui::Pos2 {
        origin + self.pan + egui::Vec2::new(p.x, p.y) * self.zoom
    }

    /// 屏幕坐标 → 画布坐标。与 [`Self::to_screen`] 互为逆运算。
    pub fn to_canvas(&self, screen: egui::Pos2, origin: egui::Pos2) -> Pos {
        let v = (screen - origin - self.pan) / self.zoom;
        Pos::new(v.x, v.y)
    }

    /// 把屏幕位移换算成画布位移。
    ///
    /// 除以 zoom 是必须的：放大 2× 时鼠标拖 10 像素，画布上只该移动 5 单位，
    /// 否则节点会跑得比鼠标快一倍。
    pub fn screen_delta_to_canvas(&self, delta: egui::Vec2) -> egui::Vec2 {
        delta / self.zoom
    }

    /// 按滚轮调整缩放，并夹在 [`MIN_ZOOM`]..=[`MAX_ZOOM`]。
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    }
}

/// 节点卡片的尺寸（画布单位）。固定尺寸让布局可预测——卡片大小随文字变
/// 会让「位置即语义」失效，因为同一位置在不同缩放下含义会漂。
pub const CARD_W: f32 = 160.0;
pub const CARD_H: f32 = 64.0;

/// 节点的画布矩形。
pub fn card_rect(p: Pos) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(p.x, p.y),
        egui::vec2(CARD_W, CARD_H),
    )
}

/// 一组节点的包围盒（画布坐标），用于「全部可见」。
///
/// 空板返回 None——调用方不该拿一个假的矩形去居中。
pub fn bounds(positions: &[Pos]) -> Option<egui::Rect> {
    let mut it = positions.iter();
    let first = it.next()?;
    let mut r = card_rect(*first);
    for p in it {
        r = r.union(card_rect(*p));
    }
    Some(r)
}

/// 计算把 `content`（**画布坐标**）完整放进 `viewport` 所需的相机。
///
/// `origin` 必须与 [`Camera::to_screen`] 用的是同一个值——相机是相对 origin
/// 的，`pan` 里必须减掉 origin，否则视口不在屏幕原点时内容会整体偏移。
///
/// 留 5% 边距，免得节点贴着边。内容为空或视口退化时返回 None，
/// 让调用方保持原相机而不是跳到一个没意义的位置。
pub fn fit_camera(
    content: egui::Rect,
    viewport: egui::Rect,
    origin: egui::Pos2,
) -> Option<Camera> {
    if content.width() <= 0.0 || content.height() <= 0.0 {
        return None;
    }
    if viewport.width() <= 1.0 || viewport.height() <= 1.0 {
        return None;
    }
    let margin = 0.05;
    let zx = viewport.width() * (1.0 - margin) / content.width();
    let zy = viewport.height() * (1.0 - margin) / content.height();
    let zoom = zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM);

    // 让内容中心对齐视口中心：解 origin + pan + c*zoom = viewport.center()。
    let c = content.center().to_vec2();
    let v = viewport.center().to_vec2();
    let pan = v - origin.to_vec2() - c * zoom;
    Some(Camera { pan, zoom })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> egui::Pos2 {
        egui::pos2(0.0, 0.0)
    }

    #[test]
    fn screen_and_canvas_transforms_are_inverse() {
        let cam = Camera {
            pan: egui::vec2(30.0, -12.5),
            zoom: 1.75,
        };
        let p = Pos::new(123.5, -40.25);
        let s = cam.to_screen(p, origin());
        let back = cam.to_canvas(s, origin());
        assert!((back.x - p.x).abs() < 1e-3, "x 往返应相等：{back:?} vs {p:?}");
        assert!((back.y - p.y).abs() < 1e-3, "y 往返应相等：{back:?} vs {p:?}");
    }

    /// 拖动换算：放大时鼠标位移要**除以** zoom，否则节点跑得比鼠标快。
    #[test]
    fn drag_delta_is_scaled_by_zoom() {
        let cam = Camera {
            pan: egui::Vec2::ZERO,
            zoom: 2.0,
        };
        let d = cam.screen_delta_to_canvas(egui::vec2(10.0, 20.0));
        assert_eq!(d, egui::vec2(5.0, 10.0), "2× 缩放时屏幕位移应减半");

        let cam = Camera {
            pan: egui::Vec2::ZERO,
            zoom: 0.5,
        };
        let d = cam.screen_delta_to_canvas(egui::vec2(10.0, 20.0));
        assert_eq!(d, egui::vec2(20.0, 40.0), "0.5× 缩放时屏幕位移应翻倍");
    }

    /// 拖动后节点应落在鼠标正下方：这是「位置即语义」的可操作含义。
    #[test]
    fn dragging_keeps_node_under_the_cursor() {
        let cam = Camera {
            pan: egui::vec2(100.0, 50.0),
            zoom: 1.5,
        };
        let start_canvas = Pos::new(20.0, 30.0);
        let start_screen = cam.to_screen(start_canvas, origin());

        // 鼠标移动 45 像素。
        let delta = egui::vec2(45.0, -15.0);
        let end_screen = start_screen + delta;

        let canvas_delta = cam.screen_delta_to_canvas(delta);
        let moved = Pos::new(
            start_canvas.x + canvas_delta.x,
            start_canvas.y + canvas_delta.y,
        );
        let painted = cam.to_screen(moved, origin());
        assert!(
            (painted - end_screen).length() < 1e-3,
            "拖动后节点应回到鼠标下：{painted:?} vs {end_screen:?}"
        );
    }

    #[test]
    fn zoom_is_clamped() {
        let mut cam = Camera::default();
        for _ in 0..50 {
            cam.zoom_by(1.5);
        }
        assert_eq!(cam.zoom, MAX_ZOOM);
        for _ in 0..100 {
            cam.zoom_by(0.5);
        }
        assert_eq!(cam.zoom, MIN_ZOOM);
    }

    #[test]
    fn bounds_of_empty_board_is_none() {
        assert!(bounds(&[]).is_none(), "空板不该编造一个矩形");
    }

    #[test]
    fn bounds_covers_all_cards() {
        let ps = [Pos::new(0.0, 0.0), Pos::new(500.0, 300.0)];
        let b = bounds(&ps).unwrap();
        assert!(b.min.x <= 0.0 && b.min.y <= 0.0);
        assert!(b.max.x >= 500.0 + CARD_W && b.max.y >= 300.0 + CARD_H);
    }

    /// fit 之后内容应完整落在视口内，且大致居中。
    #[test]
    fn fit_puts_content_inside_the_viewport() {
        let content = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 500.0));
        let viewport =
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 400.0));
        let cam = fit_camera(content, viewport, origin()).unwrap();

        // 内容四角应落在视口内。
        for p in [
            egui::pos2(content.min.x, content.min.y),
            egui::pos2(content.max.x, content.max.y),
        ] {
            let s = cam.to_screen(Pos::new(p.x, p.y), origin());
            assert!(
                viewport.contains(s),
                "内容角点应落在视口内：{s:?} 不在 {viewport:?}"
            );
        }
    }

    /// 视口不在屏幕原点时，fit 结果也必须正确。
    ///
    /// 这条是补的：`fit_camera` 一开始忘了减 origin，而旧测试用 origin=(0,0)
    /// 把 bug 盖住了。用**真实**的 to_screen 反推，而不是手写一遍公式。
    #[test]
    fn fit_respects_a_nonzero_origin() {
        let origin = egui::pos2(260.0, 96.0);
        let viewport = egui::Rect::from_min_size(origin, egui::vec2(600.0, 400.0));
        let content =
            egui::Rect::from_min_size(egui::pos2(-300.0, 120.0), egui::vec2(900.0, 700.0));
        let cam = fit_camera(content, viewport, origin).unwrap();

        for p in [
            content.min,
            content.max,
            egui::pos2(content.max.x, content.min.y),
            egui::pos2(content.min.x, content.max.y),
        ] {
            let s = cam.to_screen(Pos::new(p.x, p.y), origin);
            assert!(
                viewport.contains(s),
                "内容角点应落在视口内：{s:?} 不在 {viewport:?}（origin={origin:?}）"
            );
        }
        // 内容中心应对齐视口中心。
        let c = cam.to_screen(Pos::new(content.center().x, content.center().y), origin);
        assert!(
            (c - viewport.center()).length() < 0.5,
            "内容中心应贴近视口中心：{c:?} vs {:?}",
            viewport.center()
        );
    }

    #[test]
    fn fit_refuses_degenerate_inputs() {
        let zero = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 0.0));
        let vp = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        assert!(
            fit_camera(zero, vp, origin()).is_none(),
            "零尺寸内容不该有相机"
        );
        let content = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
        let tiny_vp = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 0.0));
        assert!(
            fit_camera(content, tiny_vp, origin()).is_none(),
            "零尺寸视口不该有相机"
        );
    }
}

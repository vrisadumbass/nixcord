//! In-app image viewer (lightbox), opened by clicking an attachment / embed image.
//!
//! A dark scrim covers the window; the image is centered and fit to about 90% of it (never
//! upscaled past its natural size). Clicking the image toggles fit / 100%, the wheel or a
//! pinch zooms toward the cursor, dragging pans while zoomed. Arrow keys or the side buttons
//! step through the other images of the same message. Top-right: copy link, save, close;
//! Esc or a click on the scrim also closes it. "Open in browser" sits under the image.
//!
//! Textures come from `Media::viewer_tex` (a large copy sized to the window, the chat
//! preview scaled up until it arrives); `Media::close_viewer` frees the large one on close.
//! The state lives in `Media::viewer`, so the app only calls [`show`] once per frame, after
//! the main UI.

use super::prop;
use crate::app::NixApp;
use crate::media::{paint_texture, Media, ViewImage, ViewTex};
use crate::theme::Theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, Modifiers, Order, Pos2, Rect,
    Response, Sense, Shape, Stroke, Ui, Vec2,
};
use nixcord_media::size;

/// Scrim opacity (about 85% black).
const SCRIM_ALPHA: u8 = 217;
/// Share of the window the fitted image may use.
const FIT_SHARE: f32 = 0.9;
/// Room under the image for the "Open in browser" link.
const LINK_ROOM: f32 = 36.0;
/// Largest zoom, relative to the natural size.
const MAX_ZOOM: f32 = 8.0;
/// Scroll points per e-fold of zoom (a wheel notch is about 50 points: ~1.2x).
const WHEEL_PER_EFOLD: f32 = 250.0;
const TOP_BTN: f32 = 36.0;
const SIDE_BTN: f32 = 44.0;

/// Open viewer state.
pub struct State {
    items: Vec<ViewImage>,
    index: usize,
    /// `None` = fit to the window; otherwise scale relative to the natural size.
    zoom: Option<f32>,
    /// Offset of the image center from the stage center while zoomed.
    pan: Vec2,
    /// Where the primary button went down on the image (click = release close by).
    press: Option<Pos2>,
}

impl State {
    pub fn new(items: Vec<ViewImage>, index: usize) -> Self {
        let index = index.min(items.len().saturating_sub(1));
        Self {
            items,
            index,
            zoom: None,
            pan: Vec2::ZERO,
            press: None,
        }
    }

    fn step(&mut self, by: isize) {
        let n = self.items.len() as isize;
        if n > 1 {
            self.index = (self.index as isize + by).rem_euclid(n) as usize;
            self.zoom = None;
            self.pan = Vec2::ZERO;
        }
    }

    /// Zoom to `target` (relative to natural size) keeping the point under `anchor` still.
    fn zoom_to(&mut self, target: f32, anchor: Pos2, stage: Rect, fit: f32) {
        let s = self.zoom.unwrap_or(fit);
        let target = target.clamp(fit, MAX_ZOOM.max(fit));
        if target <= fit * 1.001 {
            self.zoom = None;
            self.pan = Vec2::ZERO;
            return;
        }
        let center = stage.center() + self.pan;
        let rel = (anchor - center) / s;
        self.pan = (anchor - rel * target) - stage.center();
        self.zoom = Some(target);
    }

    /// Keep a zoomed image covering the window on each axis it is larger than the window;
    /// center it on the others.
    fn clamp_pan(&mut self, stage: Rect, screen: Rect, size: Vec2) {
        if self.zoom.is_none() {
            self.pan = Vec2::ZERO;
            return;
        }
        for d in 0..2 {
            let c0 = stage.center()[d];
            let c = if size[d] > screen.size()[d] {
                let lo = screen.max[d] - size[d] / 2.0;
                let hi = screen.min[d] + size[d] / 2.0;
                (c0 + self.pan[d]).clamp(lo, hi)
            } else {
                c0
            };
            self.pan[d] = c - c0;
        }
    }
}

/// Draw the viewer if one is open (or was just requested by a click). Call once per frame
/// after the main UI; also forwards finished saves to the toast.
pub fn show(ctx: &egui::Context, app: &mut NixApp) {
    let mut toasts = Vec::new();
    let mut closed = false;
    {
        let mut media = app.media.borrow_mut();
        toasts.extend(media.take_notices());
        if let Some((items, index)) = media.take_open_request() {
            media.close_viewer();
            media.viewer = Some(State::new(items, index));
        }
        if let Some(mut st) = media.viewer.take() {
            // Keys go to the viewer, not to the composer underneath.
            ctx.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
            if draw(ctx, app.theme, &mut media, &mut st, &mut toasts) {
                media.viewer = Some(st);
            } else {
                media.close_viewer();
                closed = true;
            }
        }
    }
    if closed {
        app.composer.want_focus = true;
    }
    for t in toasts {
        if !t.starts_with("Couldn't") {
            app.toast(t);
        } else {
            app.toast_err(t);
        }
    }
}

/// Size the image is laid out at when zoom is 1 (Discord reports CSS pixels = points).
fn natural_points(item: &ViewImage, tex: &ViewTex) -> Vec2 {
    if let Some((w, h)) = item.natural {
        return Vec2::new(w.max(1) as f32, h.max(1) as f32);
    }
    tex.big
        .or(tex.preview)
        .map_or(Vec2::new(640.0, 360.0), |t| {
            Vec2::new(t.size[0].max(1) as f32, t.size[1].max(1) as f32)
        })
}

/// Returns `false` when the viewer should close.
fn draw(
    ctx: &egui::Context,
    t: &Theme,
    media: &mut Media,
    st: &mut State,
    toasts: &mut Vec<String>,
) -> bool {
    let multi = st.items.len() > 1;
    let (esc, prev, next) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::Escape),
            i.consume_key(Modifiers::NONE, Key::ArrowLeft),
            i.consume_key(Modifiers::NONE, Key::ArrowRight),
        )
    });
    if esc {
        return false;
    }
    if multi && (prev || next) {
        st.step(if prev { -1 } else { 1 });
    }

    let screen = ctx.content_rect();
    let ppp = ctx.pixels_per_point();
    let win_px = (
        size::to_px(screen.width(), ppp),
        size::to_px(screen.height(), ppp),
    );
    let item = st.items[st.index].clone();
    let tex = media.viewer_tex(&item, win_px);
    let nat = natural_points(&item, &tex);
    let stage = Rect::from_center_size(
        screen.center() - Vec2::new(0.0, LINK_ROOM / 2.0),
        Vec2::new(
            (screen.width() * FIT_SHARE).max(32.0),
            (screen.height() * FIT_SHARE - LINK_ROOM).max(32.0),
        ),
    );
    let fit = (stage.width() / nat.x).min(stage.height() / nat.y).min(1.0);
    let image_rect =
        |st: &State| Rect::from_center_size(stage.center() + st.pan, nat * st.zoom.unwrap_or(fit));

    let mut keep = true;
    egui::Area::new(Id::unique("nixcord-image-viewer"))
        .order(Order::Foreground)
        .fixed_pos(screen.min)
        .constrain(false)
        .movable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            let scrim = ui.allocate_rect(screen, Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(SCRIM_ALPHA));

            // ---- zoom / pan ----
            let rect = image_rect(st);
            let img = ui.interact(
                rect.intersect(screen),
                ui.scope_id().with("image"),
                Sense::click_and_drag(),
            );
            let (pinch, wheel, hover) = ui.input(|i| {
                (
                    i.zoom_delta(),
                    i.smooth_scroll_delta.y,
                    i.pointer.hover_pos(),
                )
            });
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
            let factor = pinch * (wheel / WHEEL_PER_EFOLD).exp();
            if (factor - 1.0).abs() > 1e-4 {
                if let Some(p) = hover {
                    let s = st.zoom.unwrap_or(fit);
                    st.zoom_to(s * factor, p, stage, fit);
                }
            }
            // Clicks are detected by hand: with a drag sense, egui reports no click when press
            // and release land in the same frame (fast touchpad taps).
            let (pressed, released, ppos) = ui.input(|i| {
                (
                    i.pointer.primary_pressed(),
                    i.pointer.primary_released(),
                    i.pointer.interact_pos(),
                )
            });
            if pressed && img.contains_pointer() {
                st.press = ppos;
            }
            let clicked = released
                && st
                    .press
                    .take()
                    .zip(ppos)
                    .is_some_and(|(a, b)| a.distance(b) < 6.0);
            if clicked {
                let p = ppos.unwrap_or(rect.center());
                if st.zoom.is_none() {
                    // Fit -> 100% (or 2x when the image already shows at 100%).
                    let target = if fit < 0.99 { 1.0 } else { 2.0 };
                    st.zoom_to(target, p, stage, fit);
                } else {
                    st.zoom = None;
                }
            }
            if img.dragged() && st.zoom.is_some() {
                st.pan += img.drag_delta();
            }
            let rect = image_rect(st);
            st.clamp_pan(stage, screen, rect.size());
            let rect = image_rect(st);
            if img.hovered() {
                ctx.set_cursor_icon(match st.zoom {
                    None => CursorIcon::ZoomIn,
                    Some(_) if img.dragged() => CursorIcon::Grabbing,
                    Some(_) => CursorIcon::Grab,
                });
            }

            // ---- image ----
            let painter = ui.painter();
            match tex.big.or(tex.preview) {
                Some(tx) => paint_texture(ui, rect, CornerRadius::ZERO, tx, false),
                None => {
                    painter.rect_filled(rect, CornerRadius::same(4), Color32::from_white_alpha(8));
                }
            }
            if tex.big.is_none() {
                if tex.failed {
                    if tex.preview.is_none() {
                        painter.text(
                            rect.center(),
                            Align2::CENTER_CENTER,
                            "Couldn't load this image",
                            prop(14.0),
                            Color32::from_gray(200),
                        );
                    }
                } else {
                    let at = if tex.preview.is_some() {
                        rect.right_top() + Vec2::new(-22.0, 22.0)
                    } else {
                        rect.center()
                    };
                    egui::Spinner::new()
                        .size(20.0)
                        .color(Color32::from_gray(230))
                        .paint_at(ui, Rect::from_center_size(at, Vec2::splat(20.0)));
                }
            }

            // ---- GIF / video: first frame only ----
            if item.is_animated() {
                let r = Rect::from_center_size(rect.center(), Vec2::new(214.0, 40.0));
                let resp = ui.interact(r, ui.scope_id().with("play"), Sense::click());
                let fill = if resp.hovered() {
                    t.accent
                } else {
                    t.accent.gamma_multiply(0.85)
                };
                ui.painter()
                    .rect_filled(r, CornerRadius::same(20), fill.to_opaque());
                let tri = r.left_center() + Vec2::new(22.0, 0.0);
                ui.painter().add(Shape::convex_polygon(
                    vec![
                        tri + Vec2::new(-4.0, -6.5),
                        tri + Vec2::new(7.0, 0.0),
                        tri + Vec2::new(-4.0, 6.5),
                    ],
                    t.on_accent,
                    Stroke::NONE,
                ));
                ui.painter().text(
                    r.center() + Vec2::new(10.0, 0.0),
                    Align2::CENTER_CENTER,
                    "Open in browser to play",
                    prop(14.0),
                    t.on_accent,
                );
                if resp
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .on_hover_text("Animated images show their first frame here")
                    .clicked()
                {
                    ctx.open_url(egui::OpenUrl::new_tab(&item.url));
                }
            }

            // ---- "Open in browser" under the image (fit mode) ----
            if st.zoom.is_none() {
                let pos = Pos2::new(
                    rect.left().max(screen.left() + 16.0),
                    (rect.bottom() + 8.0).min(screen.bottom() - 26.0),
                );
                if link(ui, pos, "Open in browser").clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(&item.url));
                }
            }

            // ---- top bar: name / size / position, then the buttons ----
            let mut info = String::new();
            if let Some((w, h)) = item.natural {
                info = format!("{w} \u{00D7} {h}");
            }
            if multi {
                if !info.is_empty() {
                    info.push_str("  \u{00B7}  ");
                }
                info.push_str(&format!("{} / {}", st.index + 1, st.items.len()));
            }
            let name = if item.filename.is_empty() {
                "Image"
            } else {
                item.filename.as_str()
            };
            let left = screen.left() + 20.0;
            let max_name_w = (screen.width() - 3.0 * (TOP_BTN + 8.0) - 80.0).max(60.0);
            let name_rect = paint_clipped(
                ui,
                Pos2::new(left, screen.top() + 16.0),
                name,
                super::bold(15.0),
                Color32::from_gray(240),
                max_name_w,
            );
            if !info.is_empty() {
                ui.painter().text(
                    Pos2::new(left, name_rect.bottom() + 2.0),
                    Align2::LEFT_TOP,
                    info,
                    prop(12.5),
                    Color32::from_gray(170),
                );
            }

            let mut x = screen.right() - 16.0 - TOP_BTN;
            let y = screen.top() + 14.0;
            let mut top = |glyph: Glyph, tip: &str| {
                let r = Rect::from_min_size(Pos2::new(x, y), Vec2::splat(TOP_BTN));
                x -= TOP_BTN + 8.0;
                round_button(ui, r, glyph, tip)
            };
            if top(Glyph::Close, "Close (Esc)").clicked() {
                keep = false;
            }
            if top(Glyph::Save, "Save image").clicked() {
                media.save(&item);
            }
            if top(Glyph::Link, "Copy link").clicked() {
                ctx.copy_text(item.url.clone());
                toasts.push("Link copied to clipboard.".into());
            }

            // ---- previous / next ----
            if multi {
                let cy = screen.center().y;
                let l = Rect::from_center_size(
                    Pos2::new(screen.left() + 16.0 + SIDE_BTN / 2.0, cy),
                    Vec2::splat(SIDE_BTN),
                );
                let r = Rect::from_center_size(
                    Pos2::new(screen.right() - 16.0 - SIDE_BTN / 2.0, cy),
                    Vec2::splat(SIDE_BTN),
                );
                if round_button(ui, l, Glyph::Prev, "Previous (\u{2190})").clicked() {
                    st.step(-1);
                }
                if round_button(ui, r, Glyph::Next, "Next (\u{2192})").clicked() {
                    st.step(1);
                }
            }

            if scrim.clicked() {
                keep = false;
            }
        });
    keep
}

/// Small muted link, underlined and brighter on hover (Discord's "Open in browser").
fn link(ui: &Ui, pos: Pos2, text: &str) -> Response {
    let font = prop(13.5);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let rect = Rect::from_min_size(pos, galley.size());
    let resp = ui.interact(
        rect.expand(3.0),
        ui.scope_id().with("open-link"),
        Sense::click(),
    );
    let color = if resp.hovered() {
        Color32::from_gray(245)
    } else {
        Color32::from_gray(175)
    };
    ui.painter().galley(rect.min, galley, color);
    if resp.hovered() {
        ui.painter().line_segment(
            [
                Pos2::new(rect.left(), rect.bottom()),
                Pos2::new(rect.right(), rect.bottom()),
            ],
            Stroke::new(1.0, color),
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Paint one line of text cut with an ellipsis at `max_w`; returns its rect.
fn paint_clipped(ui: &Ui, pos: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> Rect {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('\u{2026}');
    let galley = ui.painter().layout_job(job);
    let rect = Rect::from_min_size(pos, galley.size());
    ui.painter().galley(pos, galley, color);
    rect
}

#[derive(Clone, Copy)]
enum Glyph {
    Close,
    Save,
    Link,
    Prev,
    Next,
}

/// Round translucent button with a painted glyph.
fn round_button(ui: &Ui, rect: Rect, glyph: Glyph, tip: &str) -> Response {
    let id = ui
        .scope_id()
        .with(("viewer-btn", rect.min.x as i32, rect.min.y as i32));
    let resp = ui.interact(rect, id, Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    p.circle_filled(
        rect.center(),
        rect.width() / 2.0,
        if hovered {
            Color32::from_white_alpha(40)
        } else {
            Color32::from_black_alpha(120)
        },
    );
    let color = if hovered {
        Color32::WHITE
    } else {
        Color32::from_gray(215)
    };
    let c = rect.center();
    let s = rect.width() * 0.5;
    let stroke = Stroke::new(1.8, color);
    match glyph {
        Glyph::Close => {
            let d = s * 0.34;
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        }
        Glyph::Save => {
            let top = c + Vec2::new(0.0, -s * 0.42);
            let tip_pt = c + Vec2::new(0.0, s * 0.16);
            p.line_segment([top, tip_pt], stroke);
            let a = s * 0.24;
            p.line_segment([tip_pt, tip_pt + Vec2::new(-a, -a)], stroke);
            p.line_segment([tip_pt, tip_pt + Vec2::new(a, -a)], stroke);
            let y = c.y + s * 0.42;
            p.line_segment(
                [Pos2::new(c.x - s * 0.4, y), Pos2::new(c.x + s * 0.4, y)],
                stroke,
            );
        }
        Glyph::Link => {
            // Two interlocked rings.
            let r = s * 0.22;
            let o = Vec2::new(r * 0.72, -r * 0.72);
            p.circle_stroke(c - o, r, stroke);
            p.circle_stroke(c + o, r, stroke);
        }
        Glyph::Prev | Glyph::Next => {
            let dir = if matches!(glyph, Glyph::Prev) {
                -1.0
            } else {
                1.0
            };
            let d = s * 0.22;
            let tip_pt = c + Vec2::new(dir * d * 0.6, 0.0);
            let st = Stroke::new(2.2, color);
            p.line_segment([tip_pt, tip_pt + Vec2::new(-dir * d, -d * 1.3)], st);
            p.line_segment([tip_pt, tip_pt + Vec2::new(-dir * d, d * 1.3)], st);
        }
    }
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: usize) -> ViewImage {
        ViewImage::for_test(&format!("https://example.com/{n}.png"), Some((800, 600)))
    }

    #[test]
    fn step_wraps_and_resets_zoom() {
        let mut st = State::new(vec![item(0), item(1), item(2)], 0);
        st.zoom = Some(2.0);
        st.pan = Vec2::new(5.0, 5.0);
        st.step(-1);
        assert_eq!(st.index, 2);
        assert!(st.zoom.is_none() && st.pan == Vec2::ZERO);
        st.step(1);
        assert_eq!(st.index, 0);
        let mut one = State::new(vec![item(0)], 5);
        assert_eq!(one.index, 0);
        one.step(1);
        assert_eq!(one.index, 0);
    }

    #[test]
    fn zoom_keeps_anchor_still() {
        let stage = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let mut st = State::new(vec![item(0)], 0);
        let fit = 0.5;
        let anchor = Pos2::new(500.0, 200.0);
        // Image point under the anchor before zooming, in natural units.
        let before = (anchor - stage.center()) / fit;
        st.zoom_to(1.0, anchor, stage, fit);
        let after = (anchor - (stage.center() + st.pan)) / 1.0;
        assert!((before - after).length() < 1e-3);
        // Zooming back to (or below) fit returns to fit mode.
        st.zoom_to(0.1, anchor, stage, fit);
        assert!(st.zoom.is_none() && st.pan == Vec2::ZERO);
        // Clamped at the top.
        st.zoom_to(100.0, anchor, stage, fit);
        assert_eq!(st.zoom, Some(MAX_ZOOM));
    }

    #[test]
    fn pan_is_clamped_to_cover_window() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        let stage = screen;
        let mut st = State::new(vec![item(0)], 0);
        st.zoom = Some(2.0);
        st.pan = Vec2::new(10_000.0, 10_000.0);
        // Wider than the window, shorter than it.
        st.clamp_pan(stage, screen, Vec2::new(1600.0, 500.0));
        assert_eq!(st.pan, Vec2::new(300.0, 0.0));
        st.zoom = None;
        st.pan = Vec2::new(4.0, 4.0);
        st.clamp_pan(stage, screen, Vec2::new(1600.0, 500.0));
        assert_eq!(st.pan, Vec2::ZERO);
    }
}

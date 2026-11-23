//! Discord-style GIF picker popup: a search box on top and a two-column masonry grid of
//! animated results below. With an empty search it shows trending GIFs.
//!
//! Results come from Discord's Tenor proxy (`GET /gifs/search`, `/gifs/trending-gifs`)
//! through `Action::SearchGifs` → `Command::SearchGifs` → `AppEvent::Gifs`; the state
//! lives in `ViewState::gifs` ([`GifState`]). Typing searches after a short pause. A click
//! sends the GIF's page URL as a message, which is what Discord does (the server then
//! attaches the gifv embed).
//!
//! Only visible cells request media, and the animation cache is byte-capped like every
//! other image (see `crate::media`).

use super::{paint_search, prop, Action};
use crate::app::NixApp;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use nixcord_model::GifResult;
use std::time::{Duration, Instant};

pub const WIDTH: f32 = 424.0;
pub const HEIGHT: f32 = 460.0;
const TOP_H: f32 = 52.0;
const PAD: f32 = 8.0;
const GAP: f32 = 6.0;
/// Search after the user stops typing for this long.
const DEBOUNCE: Duration = Duration::from_millis(350);
/// At most this many results are kept (Discord returns ~50).
pub const MAX_RESULTS: usize = 40;

/// GIF picker state (in `ViewState`; results arrive as `AppEvent::Gifs`).
#[derive(Default)]
pub struct GifState {
    /// The search box text.
    pub query: String,
    /// When the query last changed (for the debounce).
    pub edited: Option<Instant>,
    /// What the last request asked for: `Some(None)` = trending, `None` = nothing yet.
    pub requested: Option<Option<String>>,
    /// Id of the newest request; older responses are ignored.
    pub request_id: u64,
    pub loading: bool,
    pub results: Vec<GifResult>,
    pub error: Option<String>,
    focus_done: bool,
}

impl GifState {
    /// Apply a response; stale ones (an older `request_id`) are dropped.
    pub fn apply(&mut self, request_id: u64, result: Result<Vec<GifResult>, String>) {
        if request_id != self.request_id {
            return;
        }
        self.loading = false;
        match result {
            Ok(mut gifs) => {
                gifs.retain(|g| !g.url.is_empty() && g.width > 0 && g.height > 0);
                gifs.truncate(MAX_RESULTS);
                self.results = gifs;
                self.error = None;
            }
            Err(e) => {
                self.results.clear();
                self.error = Some(e);
            }
        }
    }

    /// Forget the search and results (when the picker closes), freeing their memory.
    pub fn reset(&mut self) {
        *self = Self {
            request_id: self.request_id,
            ..Self::default()
        };
    }

    /// The request the picker wants now, if it differs from the last one and the
    /// debounce has passed. `Err(wait)` = ask again after `wait`.
    fn wanted(&self, now: Instant) -> Result<Option<Option<String>>, Duration> {
        let q = self.query.trim();
        let want = (!q.is_empty()).then(|| q.to_string());
        if self.requested.as_ref() == Some(&want) {
            return Ok(None);
        }
        // The first (trending) request goes out right away; typing waits for a pause.
        if self.requested.is_some() {
            if let Some(edited) = self.edited {
                let since = now.saturating_duration_since(edited);
                if since < DEBOUNCE {
                    return Err(DEBOUNCE - since);
                }
            }
        }
        Ok(Some(want))
    }
}

pub struct Outcome {
    /// The page URL of the GIF that was clicked.
    pub picked: Option<String>,
    /// Escape, a click outside, or a pick.
    pub close: bool,
}

fn place(ctx: &egui::Context, anchor: Rect) -> Pos2 {
    let screen = ctx.content_rect();
    let x = (anchor.right() - WIDTH).clamp(
        screen.left() + 8.0,
        (screen.right() - WIDTH - 8.0).max(screen.left() + 8.0),
    );
    let over = anchor.top() - HEIGHT - 6.0;
    let y = if over >= screen.top() + 8.0 {
        over
    } else {
        (screen.bottom() - HEIGHT - 8.0).max(screen.top() + 8.0)
    };
    Pos2::new(x, y)
}

/// Lay out `items` (natural sizes) in two columns of width `col_w`: each goes in the
/// shorter column. Returns each item's rect (relative to the grid's top-left) and the
/// total height.
fn masonry(sizes: &[(u32, u32)], col_w: f32) -> (Vec<Rect>, f32) {
    let mut heights = [0.0f32; 2];
    let mut rects = Vec::with_capacity(sizes.len());
    for &(w, h) in sizes {
        let col = usize::from(heights[1] < heights[0]);
        let item_h = (col_w * h as f32 / w.max(1) as f32).clamp(48.0, col_w * 2.0);
        let x = col as f32 * (col_w + GAP);
        rects.push(Rect::from_min_size(
            Pos2::new(x, heights[col]),
            Vec2::new(col_w, item_h),
        ));
        heights[col] += item_h + GAP;
    }
    (rects, heights[0].max(heights[1]))
}

/// Draw the picker. Call every frame while it's open.
pub fn show(
    ctx: &egui::Context,
    app: &mut NixApp,
    anchor: Rect,
    actions: &mut Vec<Action>,
) -> Outcome {
    let t = app.theme;
    match app.view.gifs.wanted(Instant::now()) {
        Ok(Some(want)) => {
            app.view.gifs.requested = Some(want.clone());
            app.view.gifs.loading = true;
            actions.push(Action::SearchGifs(want));
        }
        Ok(None) => {}
        Err(wait) => ctx.request_repaint_after(wait),
    }
    let panel = super::presence::opaque_bg(t, t.sidebar);
    let field_fill = super::presence::opaque_bg(t, t.rail);
    let mut picked: Option<String> = None;

    let area = egui::Area::new(Id::unique("nixcord-gif-picker-area"))
        .order(egui::Order::Foreground)
        .fixed_pos(place(ctx, anchor))
        .show(ctx, |ui| {
            Frame::new()
                .fill(panel)
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(8))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(110),
                })
                .show(ui, |ui| {
                    let (outer, _) =
                        ui.allocate_exact_size(Vec2::new(WIDTH, HEIGHT), Sense::hover());
                    let top = Rect::from_min_size(outer.min, Vec2::new(WIDTH, TOP_H));
                    ui.painter().hline(
                        outer.x_range(),
                        top.bottom() - 0.5,
                        Stroke::new(1.0, t.divider),
                    );
                    search_bar(ui, app, top, field_fill);
                    let grid = Rect::from_min_max(
                        Pos2::new(outer.left() + PAD, top.bottom() + PAD),
                        Pos2::new(outer.right() - PAD, outer.bottom() - PAD),
                    );
                    picked = grid_ui(ui, app, grid);
                });
        });

    let rect = area.response.rect;
    let close = picked.is_some()
        || ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || (i.pointer.any_pressed()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| !rect.contains(p) && !anchor.contains(p)))
        });
    if close {
        app.view.gifs.reset();
    }
    Outcome { picked, close }
}

fn search_bar(ui: &mut Ui, app: &mut NixApp, top: Rect, fill: Color32) {
    let t = app.theme;
    let field = Rect::from_min_max(
        top.min + Vec2::new(12.0, 10.0),
        Pos2::new(top.right() - 12.0, top.bottom() - 10.0),
    );
    ui.painter().rect_filled(field, CornerRadius::same(4), fill);
    paint_search(
        ui,
        Pos2::new(field.right() - 14.0, field.center().y),
        14.0,
        t.muted,
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                field.min + Vec2::new(8.0, 0.0),
                field.max - Vec2::new(28.0, 0.0),
            ))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let st = &mut app.view.gifs;
    let resp = child.add(
        egui::TextEdit::singleline(&mut st.query)
            .id(Id::unique("nixcord-gif-search"))
            .hint_text("Search Tenor")
            .frame(Frame::NONE)
            .margin(Margin::ZERO)
            .font(prop(14.0))
            .desired_width(f32::INFINITY),
    );
    if !st.focus_done {
        st.focus_done = true;
        resp.request_focus();
    }
    if resp.changed() {
        st.edited = Some(Instant::now());
    }
}

/// The results grid; returns the clicked GIF's page URL.
fn grid_ui(ui: &mut Ui, app: &NixApp, grid: Rect) -> Option<String> {
    let t = app.theme;
    let st = &app.view.gifs;
    let message = if st.loading && st.results.is_empty() {
        Some("Loading GIFs…".to_string())
    } else if let Some(e) = &st.error {
        Some(e.clone())
    } else if st.results.is_empty() && st.requested.is_some() && !st.loading {
        Some("No GIFs found. Try another search.".to_string())
    } else {
        None
    };
    if let Some(msg) = message {
        ui.painter().text(
            grid.center(),
            Align2::CENTER_CENTER,
            msg,
            prop(14.0),
            t.muted,
        );
        return None;
    }

    let col_w = (grid.width() - GAP - 10.0) / 2.0;
    let sizes: Vec<(u32, u32)> = st.results.iter().map(|g| (g.width, g.height)).collect();
    let (rects, total_h) = masonry(&sizes, col_w);
    let mut picked = None;
    let mut child = ui.new_child(UiBuilder::new().max_rect(grid));
    egui::ScrollArea::vertical()
        .id_salt("nixcord-gif-grid")
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            let (area, _) =
                ui.allocate_exact_size(Vec2::new(grid.width() - 10.0, total_h), Sense::hover());
            for (i, (g, r)) in st.results.iter().zip(&rects).enumerate() {
                let r = r.translate(area.min.to_vec2());
                if !ui.is_rect_visible(r) {
                    continue;
                }
                let resp = ui.interact(r, ui.scope_id().with(("gif", i)), Sense::click());
                ui.painter()
                    .rect_filled(r, CornerRadius::same(6), t.elevated);
                let poster = g
                    .preview
                    .as_deref()
                    .or(g.gif_src.as_deref())
                    .unwrap_or(&g.src);
                let anim = nixcord_media::anim_url::gifv_candidates(
                    Some(&g.url),
                    g.gif_src.as_deref().or(g.preview.as_deref()),
                    Some(&g.src),
                );
                let mut cell = ui.new_child(UiBuilder::new().max_rect(r));
                app.media.borrow_mut().gifv(
                    &mut cell,
                    poster,
                    &anim,
                    r.size(),
                    Some((g.width, g.height)),
                );
                if resp.hovered() {
                    ui.painter().rect_stroke(
                        r,
                        CornerRadius::same(6),
                        Stroke::new(2.0, t.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    picked = Some(g.url.clone());
                }
            }
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masonry_fills_the_shorter_column() {
        let (rects, h) = masonry(&[(100, 100), (100, 50), (100, 50), (100, 100)], 100.0);
        assert_eq!(rects[0].min, Pos2::new(0.0, 0.0));
        assert_eq!(rects[1].min.x, 100.0 + GAP);
        // Column 1 is shorter after item 1, so item 2 goes under it.
        assert_eq!(rects[2].min.x, 100.0 + GAP);
        // Then column 0 (106) is shorter than column 1 (112).
        assert_eq!(rects[3].min.x, 0.0);
        assert!(h >= 200.0);
    }

    #[test]
    fn debounces_typing_but_not_the_first_request() {
        let now = Instant::now();
        let mut st = GifState::default();
        assert_eq!(st.wanted(now), Ok(Some(None)), "trending right away");
        st.requested = Some(None);
        assert_eq!(st.wanted(now), Ok(None));
        st.query = "cat".into();
        st.edited = Some(now);
        assert!(st.wanted(now).is_err(), "waits while typing");
        assert_eq!(st.wanted(now + DEBOUNCE), Ok(Some(Some("cat".to_string()))));
    }

    #[test]
    fn stale_responses_are_ignored() {
        let mut st = GifState {
            request_id: 2,
            loading: true,
            ..Default::default()
        };
        st.apply(1, Ok(vec![GifResult::default()]));
        assert!(st.loading);
        let g = GifResult {
            url: "https://tenor.com/view/x-gif-1".into(),
            width: 10,
            height: 10,
            ..Default::default()
        };
        st.apply(2, Ok(vec![g, GifResult::default()]));
        assert!(!st.loading);
        assert_eq!(st.results.len(), 1, "results without a size are dropped");
    }
}

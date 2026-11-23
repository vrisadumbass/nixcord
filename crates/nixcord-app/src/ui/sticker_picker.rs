//! Discord-style sticker picker popup: a search box on top and a grid of server stickers,
//! one section per server (the current server first). A click sends the sticker.
//!
//! Lottie stickers (vector animations) have no image nixcord can draw, so they show their
//! name in the cell instead. Sending a sticker from another server needs Nitro; Discord
//! refuses it otherwise and the error shows as a toast.

use super::{bold, paint_search, prop};
use crate::app::NixApp;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use nixcord_model::{Snowflake, Sticker};

pub const WIDTH: f32 = 424.0;
pub const HEIGHT: f32 = 440.0;
const TOP_H: f32 = 52.0;
const PAD: f32 = 8.0;
const CELL: f32 = 96.0;
const COLS: usize = 4;
const SECTION_H: f32 = 28.0;
/// Sticker images are requested at this size (px).
const STICKER_PX: u32 = 160;

#[derive(Clone, Default)]
struct State {
    query: String,
    focus_done: bool,
}

pub struct Outcome {
    pub picked: Option<Snowflake>,
    /// Escape, a click outside, or a pick.
    pub close: bool,
}

fn state_id() -> Id {
    Id::unique("nixcord-sticker-picker")
}

/// Forget the search text (when the picker closes).
pub fn reset(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<State>(state_id()));
}

/// Does `s` match the (lowercase) query by name or tag?
fn matches(s: &Sticker, q: &str) -> bool {
    q.is_empty()
        || s.name.to_lowercase().contains(q)
        || s.tags
            .split(',')
            .any(|tag| tag.trim().to_lowercase().contains(q))
}

/// `(server name, stickers)` sections: the current server first, then the rest in rail
/// order; only available stickers matching `q`.
fn sections<'a>(
    app: &'a NixApp,
    current: Option<Snowflake>,
    q: &str,
) -> Vec<(&'a str, Vec<&'a Sticker>)> {
    let mut guilds: Vec<_> = app
        .store
        .guilds()
        .into_iter()
        .filter(|g| !g.unavailable)
        .collect();
    guilds.sort_by_key(|g| Some(g.id) != current);
    guilds
        .into_iter()
        .filter_map(|g| {
            let list: Vec<&Sticker> = g
                .stickers
                .iter()
                .filter(|s| s.available && matches(s, q))
                .collect();
            (!list.is_empty()).then_some((g.name.as_str(), list))
        })
        .collect()
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

/// Draw the picker. Call every frame while it's open. `current` is the open channel's
/// server (its stickers come first).
pub fn show(
    ctx: &egui::Context,
    app: &NixApp,
    anchor: Rect,
    current: Option<Snowflake>,
) -> Outcome {
    let t = app.theme;
    let mut st: State = ctx.data(|d| d.get_temp(state_id())).unwrap_or_default();
    let panel = super::presence::opaque_bg(t, t.sidebar);
    let field_fill = super::presence::opaque_bg(t, t.rail);
    let mut picked = None;

    let area = egui::Area::new(Id::unique("nixcord-sticker-picker-area"))
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
                    search_bar(ui, app, &mut st, top, field_fill);
                    let grid = Rect::from_min_max(
                        Pos2::new(outer.left() + PAD, top.bottom()),
                        Pos2::new(outer.right() - PAD, outer.bottom() - PAD),
                    );
                    let q = st.query.trim().to_lowercase();
                    picked = grid_ui(ui, app, grid, &sections(app, current, &q));
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
        reset(ctx);
    } else {
        ctx.data_mut(|d| d.insert_temp(state_id(), st));
    }
    Outcome { picked, close }
}

fn search_bar(ui: &mut Ui, app: &NixApp, st: &mut State, top: Rect, fill: Color32) {
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
    let resp = child.add(
        egui::TextEdit::singleline(&mut st.query)
            .id(Id::unique("nixcord-sticker-search"))
            .hint_text("Find the perfect sticker")
            .frame(Frame::NONE)
            .margin(Margin::ZERO)
            .font(prop(14.0))
            .desired_width(f32::INFINITY),
    );
    if !st.focus_done {
        st.focus_done = true;
        resp.request_focus();
    }
}

/// The sticker grid; returns the clicked sticker.
fn grid_ui(
    ui: &mut Ui,
    app: &NixApp,
    grid: Rect,
    secs: &[(&str, Vec<&Sticker>)],
) -> Option<Snowflake> {
    let t = app.theme;
    if secs.is_empty() {
        ui.painter().text(
            grid.center(),
            Align2::CENTER_CENTER,
            "No stickers found",
            prop(14.0),
            t.muted,
        );
        return None;
    }
    let cell_w = ((grid.width() - 10.0) / COLS as f32).min(CELL);
    let total_h: f32 = secs
        .iter()
        .map(|(_, list)| SECTION_H + list.len().div_ceil(COLS) as f32 * cell_w)
        .sum();
    let mut picked = None;
    let mut child = ui.new_child(UiBuilder::new().max_rect(grid));
    egui::ScrollArea::vertical()
        .id_salt("nixcord-sticker-grid")
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            let (area, _) =
                ui.allocate_exact_size(Vec2::new(grid.width() - 10.0, total_h), Sense::hover());
            let mut y = area.top();
            for (name, list) in secs {
                ui.painter().text(
                    Pos2::new(area.left() + 4.0, y + SECTION_H / 2.0),
                    Align2::LEFT_CENTER,
                    name.to_uppercase(),
                    bold(12.0),
                    t.muted,
                );
                y += SECTION_H;
                for (i, s) in list.iter().enumerate() {
                    let (row, col) = (i / COLS, i % COLS);
                    let cell = Rect::from_min_size(
                        Pos2::new(area.left() + col as f32 * cell_w, y + row as f32 * cell_w),
                        Vec2::splat(cell_w),
                    );
                    if !ui.is_rect_visible(cell) {
                        continue;
                    }
                    let resp =
                        ui.interact(cell, ui.scope_id().with(("sticker", s.id)), Sense::click());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(cell, CornerRadius::same(6), t.hover);
                    }
                    let inner = cell.shrink(6.0);
                    match s.item().url(STICKER_PX) {
                        Some(url) => {
                            let mut c = ui.new_child(UiBuilder::new().max_rect(inner));
                            app.media.borrow_mut().image_preview(
                                &mut c,
                                &url,
                                inner.size(),
                                Some((STICKER_PX, STICKER_PX)),
                            );
                        }
                        None => {
                            ui.painter().text(
                                inner.center(),
                                Align2::CENTER_CENTER,
                                &s.name,
                                prop(12.0),
                                t.muted,
                            );
                        }
                    }
                    let resp = resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(&s.name);
                    if resp.clicked() {
                        picked = Some(s.id);
                    }
                }
                y += list.len().div_ceil(COLS) as f32 * cell_w;
            }
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_name_and_tags() {
        let s = Sticker {
            name: "Ferris Wave".into(),
            tags: "crab, hello".into(),
            ..Default::default()
        };
        assert!(matches(&s, ""));
        assert!(matches(&s, "wave"));
        assert!(matches(&s, "crab"));
        assert!(!matches(&s, "cat"));
    }
}

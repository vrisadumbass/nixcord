//! UI building blocks shared by the screens, plus the `Action` type the screens emit.
//!
//! Screens never mutate app-level state directly: they push `Action`s, which `app.rs`
//! applies after the frame's UI has been built. That keeps borrows simple (the store is
//! only read while drawing).

pub mod chat;
pub mod composer;
pub mod login;
pub mod settings;
pub mod sidebar;

use crate::theme::Theme;
use chrono::{Datelike, Local, TimeZone};
use eframe::egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Response, Sense, Stroke, Ui,
    Vec2,
};
use nixcord_model::{Snowflake, User};
use nixcord_store::Store;

/// Things the UI asks the app to do.
#[derive(Debug, Clone)]
pub enum Action {
    Login {
        token: String,
        remember: bool,
    },
    Logout,
    SelectHome,
    SelectGuild(Snowflake),
    SelectChannel(Snowflake),
    ToggleCategory(Snowflake),
    LoadOlder {
        channel: Snowflake,
        before: Snowflake,
    },
    RetryHistory(Snowflake),
    Send {
        channel: Snowflake,
        content: String,
    },
    Edit {
        channel: Snowflake,
        message: Snowflake,
        content: String,
    },
    StartEdit {
        message: Snowflake,
        content: String,
    },
    StartReply(Snowflake),
    AskDelete {
        channel: Snowflake,
        message: Snowflake,
    },
    Delete {
        channel: Snowflake,
        message: Snowflake,
    },
    Copy(String),
    Typing(Snowflake),
    OpenSettings,
}

/// Bold-ish family (Segoe UI Semibold on Windows when available, else the default font).
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn prop(size: f32) -> FontId {
    FontId::proportional(size)
}

// ---- colors -----------------------------------------------------------------------

const AVATAR_COLORS: [u32; 8] = [
    0x5865f2, 0x3ba55c, 0xed4245, 0xfaa61a, 0xeb459e, 0x747f8d, 0x1abc9c, 0x9b59b6,
];

/// Stable placeholder avatar color for a user/guild id.
pub fn id_color(id: Snowflake) -> Color32 {
    let h = id.0 ^ (id.0 >> 22) ^ (id.0 >> 41);
    let c = AVATAR_COLORS[(h % AVATAR_COLORS.len() as u64) as usize];
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Color of the member's highest colored role in `guild`.
///
/// Returns `None` until the model carries guild member roles on messages (planned); callers
/// then fall back to the theme's default name color.
pub fn role_color(_store: &Store, _guild: Option<Snowflake>, _user: Snowflake) -> Option<Color32> {
    None
}

// ---- painting helpers ---------------------------------------------------------------

pub fn initials(name: &str, max: usize) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let mut s: String = if words.len() > 1 {
        words
            .iter()
            .filter_map(|w| w.chars().next())
            .take(max)
            .collect()
    } else {
        name.chars()
            .filter(|c| c.is_alphanumeric())
            .take(max.min(2))
            .collect()
    };
    if s.is_empty() {
        s = name.chars().take(1).collect();
    }
    if max == 1 {
        s = s.chars().take(1).collect();
    }
    s.to_uppercase()
}

/// Placeholder avatar: colored circle with the first letter of the name.
/// (Avatar images slot in here later via a texture cache.)
pub fn paint_avatar(ui: &Ui, center: Pos2, diameter: f32, user: &User) {
    let p = ui.painter();
    p.circle_filled(center, diameter / 2.0, id_color(user.id));
    p.text(
        center,
        Align2::CENTER_CENTER,
        initials(user.display_name(), 1),
        bold(diameter * 0.42),
        Color32::WHITE,
    );
}

pub fn paint_status_dot(
    ui: &Ui,
    avatar_center: Pos2,
    diameter: f32,
    ring: Color32,
    color: Color32,
) {
    let r = diameter * 0.16;
    let c = avatar_center + Vec2::splat(diameter * 0.5 - r * 0.9);
    ui.painter().circle_filled(c, r + 2.5, ring);
    ui.painter().circle_filled(c, r, color);
}

pub fn paint_chevron(ui: &Ui, center: Pos2, size: f32, down: bool, color: Color32) {
    let s = size / 2.0;
    let stroke = Stroke::new(1.5, color);
    let pts = if down {
        [
            center + Vec2::new(-s, -s * 0.5),
            center + Vec2::new(0.0, s * 0.5),
            center + Vec2::new(s, -s * 0.5),
        ]
    } else {
        [
            center + Vec2::new(-s * 0.5, -s),
            center + Vec2::new(s * 0.5, 0.0),
            center + Vec2::new(-s * 0.5, s),
        ]
    };
    ui.painter().line_segment([pts[0], pts[1]], stroke);
    ui.painter().line_segment([pts[1], pts[2]], stroke);
}

pub fn paint_plus(ui: &Ui, center: Pos2, size: f32, color: Color32) {
    let s = size / 2.0;
    let stroke = Stroke::new(2.0, color);
    ui.painter().line_segment(
        [center - Vec2::new(s, 0.0), center + Vec2::new(s, 0.0)],
        stroke,
    );
    ui.painter().line_segment(
        [center - Vec2::new(0.0, s), center + Vec2::new(0.0, s)],
        stroke,
    );
}

pub fn paint_search(ui: &Ui, center: Pos2, size: f32, color: Color32) {
    let r = size * 0.32;
    let c = center - Vec2::splat(size * 0.08);
    ui.painter().circle_stroke(c, r, Stroke::new(1.6, color));
    let d = r * std::f32::consts::FRAC_1_SQRT_2;
    ui.painter().line_segment(
        [c + Vec2::splat(d), c + Vec2::splat(d + size * 0.25)],
        Stroke::new(1.8, color),
    );
}

/// Icons: a few painted as vectors (crisp at any zoom), the rest as font glyphs.
#[derive(Clone, Copy)]
pub enum Icon {
    Mic,
    Headphones,
    Gear,
    Members,
    Copy,
    Glyph(&'static str),
}

pub fn paint_icon(ui: &Ui, icon: Icon, rect: Rect, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.5; // icon box is ~ s x s
    let stroke = Stroke::new(1.8, color);
    match icon {
        Icon::Mic => {
            let body =
                Rect::from_center_size(c - Vec2::new(0.0, s * 0.18), Vec2::new(s * 0.42, s * 0.7));
            p.rect_filled(body, CornerRadius::same((s * 0.21) as u8), color);
            let arc: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::PI * i as f32 / 12.0;
                    c + Vec2::new(-a.cos() * s * 0.38, a.sin() * s * 0.38 - s * 0.05)
                })
                .collect();
            p.add(egui::Shape::line(arc, stroke));
            p.line_segment(
                [c + Vec2::new(0.0, s * 0.33), c + Vec2::new(0.0, s * 0.5)],
                stroke,
            );
        }
        Icon::Headphones => {
            let arc: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::PI * i as f32 / 16.0;
                    c + Vec2::new(-a.cos() * s * 0.45, -a.sin() * s * 0.45 + s * 0.08)
                })
                .collect();
            p.add(egui::Shape::line(arc, Stroke::new(2.0, color)));
            for dx in [-1.0, 1.0] {
                let cup = Rect::from_center_size(
                    c + Vec2::new(dx * s * 0.38, s * 0.28),
                    Vec2::new(s * 0.2, s * 0.4),
                );
                p.rect_filled(cup, CornerRadius::same(2), color);
            }
        }
        Icon::Gear => {
            for i in 0..8 {
                let a = std::f32::consts::TAU * i as f32 / 8.0;
                let d = Vec2::new(a.cos(), a.sin());
                p.line_segment(
                    [c + d * s * 0.3, c + d * s * 0.5],
                    Stroke::new(s * 0.2, color),
                );
            }
            p.circle_stroke(c, s * 0.27, Stroke::new(s * 0.2, color));
        }
        Icon::Members => {
            for (dx, r, col) in [
                (s * 0.22, s * 0.15, color.gamma_multiply(0.7)),
                (-s * 0.1, s * 0.18, color),
            ] {
                let head = c + Vec2::new(dx, -s * 0.18);
                p.circle_filled(head, r, col);
                let body = Rect::from_center_size(
                    head + Vec2::new(0.0, r * 2.6),
                    Vec2::new(r * 3.0, r * 2.0),
                );
                p.rect_filled(
                    body,
                    CornerRadius {
                        nw: (r * 1.5) as u8,
                        ne: (r * 1.5) as u8,
                        sw: 1,
                        se: 1,
                    },
                    col,
                );
            }
        }
        Icon::Copy => {
            let back =
                Rect::from_center_size(c + Vec2::new(-s * 0.12, -s * 0.12), Vec2::splat(s * 0.62));
            let front = back.translate(Vec2::splat(s * 0.24));
            p.rect_stroke(
                back,
                CornerRadius::same(2),
                Stroke::new(1.5, color),
                egui::StrokeKind::Middle,
            );
            p.rect_filled(
                front.expand(1.0),
                CornerRadius::same(2),
                ui.visuals().window_fill,
            );
            p.rect_stroke(
                front,
                CornerRadius::same(2),
                Stroke::new(1.5, color),
                egui::StrokeKind::Middle,
            );
        }
        Icon::Glyph(g) => {
            p.text(
                c,
                Align2::CENTER_CENTER,
                g,
                prop(rect.height() * 0.55),
                color,
            );
        }
    }
}

/// Square icon button with hover background.
pub fn icon_button(ui: &mut Ui, t: &Theme, icon: Icon, size: f32, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    paint_icon(ui, icon, rect, if hovered { t.strong } else { t.muted });
    resp.on_hover_text(tooltip)
}

/// Horizontal hairline.
pub fn hline(ui: &Ui, rect: Rect, y: f32, color: Color32) {
    ui.painter().line_segment(
        [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
        Stroke::new(1.0, color),
    );
}

// ---- time formatting ----------------------------------------------------------------

fn local(ms: u64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms as i64).single()
}

pub fn fmt_clock(ms: u64, h24: bool) -> String {
    match local(ms) {
        Some(t) if h24 => t.format("%H:%M").to_string(),
        Some(t) => t.format("%-I:%M %p").to_string(),
        None => String::new(),
    }
}

/// "Today at 9:53 PM" / "Yesterday at 9:53 PM" / "11/20/2026 1:28 PM".
pub fn fmt_stamp(ms: u64, h24: bool) -> String {
    let Some(t) = local(ms) else {
        return String::new();
    };
    let today = Local::now().date_naive();
    let day = t.date_naive();
    let clock = fmt_clock(ms, h24);
    if day == today {
        format!("Today at {clock}")
    } else if today.pred_opt() == Some(day) {
        format!("Yesterday at {clock}")
    } else {
        format!("{}/{}/{} {clock}", day.month(), day.day(), day.year())
    }
}

/// Local calendar day number, for date dividers.
pub fn day_number(ms: u64) -> i32 {
    local(ms).map_or(0, |t| t.date_naive().num_days_from_ce())
}

/// "November 20, 2026".
pub fn fmt_day(ms: u64) -> String {
    local(ms).map_or_else(String::new, |t| t.format("%B %-d, %Y").to_string())
}

pub fn fmt_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else {
        format!("{:.1} MB", b / K / K)
    }
}

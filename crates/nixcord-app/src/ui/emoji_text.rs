//! Single-line text with color (Twemoji) unicode emoji: names, channel names, status lines,
//! reply previews, reaction pills.
//!
//! Emoji are found with `nixcord_emoji::emoji_runs` (grapheme clusters checked against the
//! emoji table, so digits and `#` stay text). Each emoji reserves a fixed square in the
//! galley (two invisible hairline glyphs, the same trick the markdown renderer uses for
//! custom emoji), so the layout never shifts when a texture arrives. The square shows the
//! Twemoji texture from `Media` when loaded, else the font's (monochrome) glyph.
//!
//! - `emoji_text::label(ui, &app.media, text, font, color)` allocates and paints a label.
//! - `emoji_text::layout(...)` + `EmojiGalley::paint(...)` for painter-based call sites that
//!   clip or position text themselves.
//! - `emoji_text::paint_emoji(...)` draws one emoji into a rect (reaction pills, picker).

use super::markdown::char_rects;
use crate::media::Media;
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Galley,
    Painter, Pos2, Rect, Response, Sense, TextFormat, Ui, Vec2,
};
use nixcord_emoji::{emoji_runs, TextOrEmoji};
use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

/// Emoji size in labels, relative to the font size (about the line height).
pub const LABEL_SCALE: f32 = 1.1;

/// Glyph of each of the two invisible sections that reserve an emoji's square.
pub(crate) const PLACEHOLDER: &str = "i";

/// Format of the two [`PLACEHOLDER`] sections that reserve an emoji's square in a
/// `LayoutJob`: transparent 1 pt glyphs; the second is appended with `size` points of
/// leading space. The emoji is drawn over the char range of both, so it takes about `size`
/// points and wraps like a word.
pub(crate) fn placeholder(line_height: Option<f32>) -> TextFormat {
    TextFormat {
        font_id: FontId::new(1.0, FontFamily::Proportional),
        color: Color32::TRANSPARENT,
        line_height,
        valign: Align::Center,
        ..Default::default()
    }
}

/// Append an emoji's reserved square to `job` (2 chars).
pub(crate) fn append_placeholder(
    job: &mut LayoutJob,
    lead: f32,
    size: f32,
    line_height: Option<f32>,
) {
    let f = placeholder(line_height);
    job.append(PLACEHOLDER, lead, f.clone());
    job.append(PLACEHOLDER, (size - 0.6).max(1.0), f);
}

fn is_fallback_strip(c: char) -> bool {
    c == '\u{FE0F}' || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// The emoji as the monochrome fallback font can draw it: takes the first ZWJ component and
/// strips U+FE0F and skin-tone modifiers, which the bundled monochrome emoji font lacks.
pub fn glyph(emoji: &str) -> Cow<'_, str> {
    let base = emoji.split('\u{200D}').next().unwrap_or(emoji);
    if base.len() == emoji.len() && !base.chars().any(is_fallback_strip) {
        Cow::Borrowed(emoji)
    } else if !base.chars().any(is_fallback_strip) {
        Cow::Borrowed(base)
    } else {
        Cow::Owned(base.chars().filter(|&c| !is_fallback_strip(c)).collect())
    }
}

/// Two-letter ASCII country code for a regional-indicator flag pair (`🇯🇵` -> `b"JP"`).
pub fn flag_code(emoji: &str) -> Option<[u8; 2]> {
    let mut it = emoji.chars();
    let a = it.next()?;
    let b = it.next()?;
    if it.next().is_some() {
        return None;
    }
    let to_ascii = |c: char| {
        ('\u{1F1E6}'..='\u{1F1FF}')
            .contains(&c)
            .then(|| b'A' + (c as u32 - 0x1F1E6) as u8)
    };
    Some([to_ascii(a)?, to_ascii(b)?])
}

/// Whether the loaded font family has real glyphs for every character in `s` (never tofu).
pub fn has_font_glyphs(ctx: &egui::Context, font: &FontId, s: &str) -> bool {
    !s.is_empty() && ctx.fonts_mut(|f| f.has_glyphs(font, s))
}

/// Paint a non-texture fallback for `emoji` into `rect` without ever drawing tofu boxes:
/// 2-letter flag badge for regional-indicator pairs, monochrome font glyph when covered, or
/// a clean rounded badge otherwise.
pub fn paint_fallback(painter: &Painter, rect: Rect, emoji: &str, color: Color32) {
    let size = rect.width().min(rect.height());
    if let Some(code) = flag_code(emoji) {
        let text = std::str::from_utf8(&code).unwrap_or("");
        let badge = Rect::from_center_size(rect.center(), Vec2::new(size, (size * 0.78).max(4.0)));
        painter.rect_filled(
            badge,
            CornerRadius::same((size * 0.16).round().clamp(1.0, 255.0) as u8),
            color.gamma_multiply(0.14),
        );
        painter.text(
            badge.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional((size * 0.54).max(6.0)),
            color,
        );
        return;
    }
    let g = glyph(emoji);
    let font = FontId::proportional((size * 0.8).max(6.0));
    if has_font_glyphs(painter.ctx(), &font, &g) {
        painter.text(rect.center(), Align2::CENTER_CENTER, g, font, color);
    } else {
        let badge = Rect::from_center_size(rect.center(), Vec2::splat((size * 0.86).max(4.0)));
        painter.rect_filled(
            badge,
            CornerRadius::same((size * 0.22).round().clamp(1.0, 255.0) as u8),
            color.gamma_multiply(0.14),
        );
        if let Some(ch) = nixcord_emoji::find(emoji)
            .and_then(|e| e.shortcode().chars().find(|c| c.is_ascii_alphanumeric()))
        {
            let mut buf = [0u8; 4];
            let s = ch.to_ascii_uppercase().encode_utf8(&mut buf);
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                s,
                FontId::proportional((size * 0.54).max(6.0)),
                color.gamma_multiply(0.85),
            );
        }
    }
}

/// Paint `emoji` into `rect`: the Twemoji texture, or a non-tofu fallback in `color` while
/// it loads (or if it can't).
pub fn paint_emoji(
    painter: &Painter,
    rect: Rect,
    emoji: &str,
    media: &RefCell<Media>,
    color: Color32,
) {
    if !painter.clip_rect().intersects(rect) {
        return;
    }
    let size = rect.width().min(rect.height());
    let tex = media
        .try_borrow_mut()
        .ok()
        .and_then(|mut m| m.twemoji_points(emoji, size));
    match tex {
        Some(t) => {
            let r = Rect::from_center_size(rect.center(), Vec2::splat(size));
            painter.add(
                egui::epaint::RectShape::filled(r, CornerRadius::ZERO, Color32::WHITE)
                    .with_texture(t.id, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))),
            );
        }
        None => paint_fallback(painter, rect, emoji, color),
    }
}

/// [`paint_emoji`] through a `Ui` (respects its clip rect).
pub fn paint_emoji_ui(ui: &Ui, rect: Rect, emoji: &str, media: &RefCell<Media>, color: Color32) {
    paint_emoji(ui.painter(), rect, emoji, media, color);
}

/// Laid-out text plus where its emoji go.
pub struct EmojiGalley {
    pub galley: Arc<Galley>,
    /// (char range of the placeholder in the galley, emoji)
    emoji: Vec<(Range<usize>, Box<str>)>,
    color: Color32,
}

impl EmojiGalley {
    pub fn size(&self) -> Vec2 {
        self.galley.size()
    }

    /// Paint at `pos` (top-left) with `painter` (its clip rect applies to the emoji too).
    pub fn paint(&self, painter: &Painter, pos: Pos2, media: &RefCell<Media>) {
        painter.galley(pos, self.galley.clone(), self.color);
        let mut rects = Vec::new();
        for (range, e) in &self.emoji {
            rects.clear();
            char_rects(&self.galley, range, pos, &mut rects);
            if let Some(r) = rects.first() {
                let size = (r.width()).min(r.height()).max(1.0);
                let square = Rect::from_center_size(r.center(), Vec2::splat(size));
                paint_emoji(painter, square, e, media, self.color);
            }
        }
    }
}

/// Lay out `text` in one style, emoji as squares of [`LABEL_SCALE`] x the font size.
/// `max_width` / `max_rows` wrap or truncate (with "…") like `LayoutJob::wrap`; pass
/// `f32::INFINITY` / `0` for a plain one-line label.
pub fn layout(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
    max_rows: usize,
) -> EmojiGalley {
    let size = (font.size * LABEL_SCALE).round();
    let mut job = LayoutJob::default();
    job.wrap.max_width = max_width;
    if max_rows > 0 {
        job.wrap.max_rows = max_rows;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
    }
    let fmt = TextFormat {
        font_id: font,
        color,
        valign: Align::Center,
        ..Default::default()
    };
    let mut emoji = Vec::new();
    let mut chars = 0;
    for run in emoji_runs(text) {
        match run {
            TextOrEmoji::Text(t) => {
                job.append(t, 0.0, fmt.clone());
                chars += t.chars().count();
            }
            TextOrEmoji::Emoji(e) => {
                append_placeholder(&mut job, 1.0, size, None);
                emoji.push((chars..chars + 2, Box::from(e)));
                chars += 2;
            }
        }
    }
    EmojiGalley {
        galley: painter.layout_job(job),
        emoji,
        color,
    }
}

/// A one-line label with color emoji. Allocates its size and returns the response.
pub fn label(
    ui: &mut Ui,
    media: &RefCell<Media>,
    text: &str,
    font: FontId,
    color: Color32,
) -> Response {
    let g = layout(ui.painter(), text, font, color, f32::INFINITY, 0);
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    if ui.is_rect_visible(rect) {
        g.paint(ui.painter(), rect.min, media);
    }
    resp
}

/// Like [`label`] but cut to the available width with "…".
pub fn label_truncated(
    ui: &mut Ui,
    media: &RefCell<Media>,
    text: &str,
    font: FontId,
    color: Color32,
) -> Response {
    let g = layout(ui.painter(), text, font, color, ui.available_width(), 1);
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    if ui.is_rect_visible(rect) {
        g.paint(ui.painter(), rect.min, media);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_emoji_squares() {
        let ctx = egui::Context::default();
        let media = RefCell::new(Media::new(&ctx, true));
        let mut widths = (0.0, 0.0);
        for _ in 0..2 {
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let plain = layout(
                    ui.painter(),
                    "ab",
                    FontId::proportional(16.0),
                    Color32::WHITE,
                    f32::INFINITY,
                    0,
                );
                let with = layout(
                    ui.painter(),
                    "ab\u{1F980}",
                    FontId::proportional(16.0),
                    Color32::WHITE,
                    f32::INFINITY,
                    0,
                );
                assert_eq!(with.emoji.len(), 1);
                widths = (plain.size().x, with.size().x);
                label(
                    ui,
                    &media,
                    "#1 \u{1F44D}\u{1F3FD} ok",
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
            });
            let mut out = out;
            out.textures_delta.clear();
        }
        // The emoji takes about LABEL_SCALE x the font size, plus its spacing.
        let extra = widths.1 - widths.0;
        assert!((14.0..26.0).contains(&extra), "{widths:?}");
    }

    #[test]
    fn plain_text_has_no_emoji() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let g = layout(
                ui.painter(),
                "#general 123",
                FontId::proportional(14.0),
                Color32::WHITE,
                f32::INFINITY,
                0,
            );
            assert!(g.emoji.is_empty());
        });
        out.textures_delta.clear();
        assert_eq!(glyph("\u{2764}\u{FE0F}"), "\u{2764}");
        assert_eq!(glyph("\u{1F44D}\u{1F3FD}"), "\u{1F44D}");
        assert_eq!(glyph("\u{2764}\u{FE0F}\u{200D}\u{1F525}"), "\u{2764}");
        assert_eq!(flag_code("\u{1F1EF}\u{1F1F5}"), Some(*b"JP"));
        assert_eq!(flag_code("\u{1F980}"), None);
        assert_eq!(flag_code("JP"), None);
    }

    #[test]
    fn fallback_never_paints_tofu_for_missing_glyphs() {
        let ctx = egui::Context::default();
        let mut missing_crab = false;
        let mut has_thumbs = false;
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let font = FontId::proportional(14.0);
                missing_crab = !has_font_glyphs(ui.ctx(), &font, "\u{1F980}");
                has_thumbs = has_font_glyphs(ui.ctx(), &font, &glyph("\u{1F44D}\u{1F3FD}"));
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(18.0));
                paint_fallback(ui.painter(), rect, "\u{1F980}", Color32::WHITE);
                paint_fallback(ui.painter(), rect, "\u{1F1EF}\u{1F1F5}", Color32::WHITE);
                paint_fallback(ui.painter(), rect, "\u{1F44D}\u{1F3FD}", Color32::WHITE);
            });
            out.textures_delta.clear();
        }
        assert!(missing_crab, "default egui fonts lack U+1F980 crab");
        assert!(has_thumbs, "default egui fonts cover U+1F44D thumbsup");
    }
}

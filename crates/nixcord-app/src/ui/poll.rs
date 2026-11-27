//! Poll card under a message: question, one row per answer, a footer with the vote count
//! and time left, and voting.
//!
//! Before I vote (and while the poll is open) answers are selectable: radio behaviour, or
//! check boxes with `allow_multiselect`, then "Vote". Afterwards, and once the poll is
//! closed, each row shows a fill bar with its share, the percentage and vote count, and a
//! check mark on my answers; an open poll I voted on offers "Remove Vote".
//!
//! Voting is not optimistic: `Action::PollVote` goes to the backend and the store changes
//! when MESSAGE_POLL_VOTE_ADD / _REMOVE arrive. Until then (at most [`PENDING_SECS`]) the
//! buttons show as pending; a failure shows the backend's error toast and the buttons come
//! back when the pending mark expires.

use super::{bold, emoji_text, prop, Action};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, StrokeKind,
    Ui, UiBuilder, Vec2,
};
use nixcord_model::{Message, Poll, PollAnswer, Snowflake};
use std::cell::RefCell;

/// Same outer width as an embed card (488 + 32, as in the official client).
const CARD_MAX_W: f32 = 520.0;
const ROW_H: f32 = 44.0;
const EMOJI: f32 = 22.0;
/// How long a sent vote shows as pending without a gateway answer.
pub const PENDING_SECS: f64 = 8.0;

/// Draw `m`'s poll, if it has one.
pub fn show(ui: &mut Ui, app: &NixApp, m: &Message, actions: &mut Vec<Action>) {
    if let Some(p) = m.poll.as_deref() {
        let now = chrono::Utc::now().timestamp();
        show_with(
            ui,
            &app.media,
            app.theme,
            m.channel_id,
            m.id,
            p,
            now,
            actions,
        );
    }
}

/// Rough height for the chat's row estimate.
pub fn estimate(m: &Message) -> f32 {
    m.poll
        .as_deref()
        .map_or(0.0, |p| 110.0 + p.answers.len() as f32 * (ROW_H + 8.0))
}

// ---- pure helpers -------------------------------------------------------------------

/// Whole percentages per answer that add up to exactly 100 (largest remainder; ties go to
/// the earlier answer). All zeros when nobody voted.
pub fn percentages(counts: &[u32]) -> Vec<u32> {
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return vec![0; counts.len()];
    }
    let mut out: Vec<u32> = counts
        .iter()
        .map(|&c| (u64::from(c) * 100 / total) as u32)
        .collect();
    let mut left = 100 - out.iter().sum::<u32>();
    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(u64::from(counts[i]) * 100 % total));
    for i in order {
        if left == 0 {
            break;
        }
        if u64::from(counts[i]) * 100 % total > 0 {
            out[i] += 1;
            left -= 1;
        }
    }
    out
}

/// Unix seconds of an ISO 8601 expiry.
pub fn expiry_unix(expiry: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(expiry)
        .ok()
        .map(|t| t.timestamp())
}

/// Finalized, or past its expiry.
pub fn is_closed(p: &Poll, now: i64) -> bool {
    p.finalized
        || p.expiry
            .as_deref()
            .and_then(expiry_unix)
            .is_some_and(|e| e <= now)
}

/// "5d left", "5h left", "12m left", "<1m left".
pub fn time_left(secs: i64) -> String {
    const M: i64 = 60;
    const H: i64 = 60 * M;
    const D: i64 = 24 * H;
    match secs {
        s if s >= D => format!("{}d left", s / D),
        s if s >= H => format!("{}h left", s / H),
        s if s >= M => format!("{}m left", s / M),
        _ => "<1m left".to_string(),
    }
}

/// Seconds until [`time_left`] shows something else (for one scheduled repaint).
fn next_change(secs: i64) -> i64 {
    let unit = match secs {
        s if s >= 86_400 => 86_400,
        s if s >= 3_600 => 3_600,
        s if s >= 60 => 60,
        s => return s.max(1),
    };
    (secs % unit).max(1)
}

/// "3 votes · 5h left", "1 vote · Poll closed".
pub fn footer_text(p: &Poll, now: i64) -> String {
    let n = p.total_votes();
    let votes = if n == 1 {
        "1 vote".to_string()
    } else {
        format!("{n} votes")
    };
    let tail = if is_closed(p, now) {
        "Poll closed".to_string()
    } else {
        match p.expiry.as_deref().and_then(expiry_unix) {
            Some(e) => time_left(e - now),
            None => return votes,
        }
    };
    format!("{votes} \u{b7} {tail}")
}

fn votes_label(n: u32) -> String {
    if n == 1 {
        "1 vote".into()
    } else {
        format!("{n} votes")
    }
}

// ---- per-message UI state -------------------------------------------------------------

fn selection_id(message: Snowflake) -> Id {
    Id::unique(("nixcord-poll-selection", message.0))
}

/// `(egui time sent, whether I had voted then)`.
fn pending_id(message: Snowflake) -> Id {
    Id::unique(("nixcord-poll-pending", message.0))
}

/// Whether a vote I sent is still waiting for its gateway events.
fn pending(ui: &Ui, message: Snowflake, p: &Poll) -> bool {
    let id = pending_id(message);
    let Some((sent, voted_before)) = ui.ctx().data(|d| d.get_temp::<(f64, bool)>(id)) else {
        return false;
    };
    let age = ui.input(|i| i.time) - sent;
    if p.me_voted() != voted_before || !(0.0..PENDING_SECS).contains(&age) {
        ui.ctx().data_mut(|d| d.remove::<(f64, bool)>(id));
        return false;
    }
    // Wake up once when the mark expires, so the buttons come back after a failure.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs_f64(PENDING_SECS - age));
    true
}

fn mark_pending(ui: &Ui, message: Snowflake, p: &Poll) {
    let now = ui.input(|i| i.time);
    ui.ctx()
        .data_mut(|d| d.insert_temp(pending_id(message), (now, p.me_voted())));
}

// ---- drawing --------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn show_with(
    ui: &mut Ui,
    media: &RefCell<Media>,
    t: &Theme,
    channel: Snowflake,
    message: Snowflake,
    p: &Poll,
    now: i64,
    actions: &mut Vec<Action>,
) {
    let closed = is_closed(p, now);
    let voted = p.me_voted();
    let voting = !closed && !voted;
    let busy = pending(ui, message, p);
    let mut selected: Vec<u32> = if voting {
        ui.ctx()
            .data(|d| d.get_temp(selection_id(message)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    selected.retain(|id| p.answers.iter().any(|a| a.id == *id));
    let pct = percentages(&p.answers.iter().map(|a| a.count).collect::<Vec<_>>());

    if !closed {
        if let Some(e) = p.expiry.as_deref().and_then(expiry_unix) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(next_change(e - now) as u64));
        }
    }

    let card_w = ui.available_width().min(CARD_MAX_W);
    let fill = if t.dark {
        Color32::from_white_alpha(8)
    } else {
        Color32::from_black_alpha(10)
    };
    ui.add_space(2.0);
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            let inner_w = card_w - 32.0;
            ui.set_width(inner_w);
            ui.spacing_mut().item_spacing.y = 8.0;

            let q = emoji_text::layout(ui.painter(), &p.question, bold(17.0), t.strong, inner_w, 0);
            let (r, _) = ui.allocate_exact_size(q.size(), Sense::hover());
            q.paint(ui.painter(), r.min, media);
            let hint = if voting {
                if p.allow_multiselect {
                    "Select one or more answers"
                } else {
                    "Select one answer"
                }
            } else if closed {
                "Final results"
            } else {
                "Results"
            };
            ui.add_space(-4.0);
            ui.label(egui::RichText::new(hint).font(prop(13.0)).color(t.muted));

            for (a, &share) in p.answers.iter().zip(&pct) {
                let is_sel = selected.contains(&a.id);
                let multi = p.allow_multiselect;
                let clicked =
                    answer_row(ui, media, t, message, a, share, voting, multi, is_sel, busy);
                if clicked {
                    if p.allow_multiselect {
                        if is_sel {
                            selected.retain(|x| *x != a.id);
                        } else {
                            selected.push(a.id);
                        }
                    } else {
                        selected = vec![a.id];
                    }
                }
            }

            // Footer: count and time on the left, the action on the right.
            ui.add_space(2.0);
            let (row, _) = ui.allocate_exact_size(Vec2::new(inner_w, 32.0), Sense::hover());
            ui.painter().text(
                Pos2::new(row.left(), row.center().y),
                Align2::LEFT_CENTER,
                footer_text(p, now),
                prop(13.0),
                t.muted,
            );
            if closed {
                return;
            }
            if voting {
                let enabled = !selected.is_empty() && !busy;
                let label = if busy { "Voting\u{2026}" } else { "Vote" };
                let r = Rect::from_min_max(
                    Pos2::new(row.right() - 88.0, row.top()),
                    row.right_bottom(),
                );
                if vote_button(ui, t, message, r, label, enabled) {
                    let mut ids = selected.clone();
                    ids.sort_unstable();
                    mark_pending(ui, message, p);
                    actions.push(Action::PollVote {
                        channel,
                        message,
                        answer_ids: ids,
                    });
                }
            } else {
                let label = if busy {
                    "Removing\u{2026}"
                } else {
                    "Remove Vote"
                };
                let g = ui
                    .painter()
                    .layout_no_wrap(label.into(), prop(13.0), t.link);
                let r = Rect::from_min_size(
                    Pos2::new(row.right() - g.size().x, row.center().y - g.size().y / 2.0),
                    g.size(),
                );
                let resp = ui.interact(
                    r,
                    Id::unique(("nixcord-poll-remove", message.0)),
                    if busy { Sense::hover() } else { Sense::click() },
                );
                let color = if busy { t.muted } else { t.link };
                ui.painter().galley(r.min, g, color);
                if resp.hovered() && !busy {
                    ui.painter()
                        .hline(r.x_range(), r.bottom(), Stroke::new(1.0, t.link));
                }
                if resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                    && !busy
                {
                    mark_pending(ui, message, p);
                    actions.push(Action::PollVote {
                        channel,
                        message,
                        answer_ids: Vec::new(),
                    });
                }
            }
        });

    if voting {
        ui.ctx()
            .data_mut(|d| d.insert_temp(selection_id(message), selected));
    }
}

/// One answer. Returns whether it was clicked (voting mode only).
#[allow(clippy::too_many_arguments)]
fn answer_row(
    ui: &mut Ui,
    media: &RefCell<Media>,
    t: &Theme,
    message: Snowflake,
    a: &PollAnswer,
    share: u32,
    voting: bool,
    multi: bool,
    selected: bool,
    busy: bool,
) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::hover());
    let resp = ui.interact(
        rect,
        Id::unique(("nixcord-poll-answer", message.0, a.id)),
        if voting && !busy {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let p = ui.painter();
    let hot = voting && !busy && resp.hovered();
    let base = if hot { t.selected } else { t.hover };
    p.rect_filled(rect, CornerRadius::same(8), base);
    if !voting && share > 0 {
        // Fill bar proportional to the answer's share.
        let fill_w = rect.width() * share as f32 / 100.0;
        let bar = Rect::from_min_size(rect.min, Vec2::new(fill_w, rect.height()));
        let c = if a.me_voted {
            t.accent.gamma_multiply(0.35)
        } else {
            t.muted.gamma_multiply(0.22)
        };
        p.rect_filled(bar, CornerRadius::same(8), c);
    }
    let border = if (voting && selected) || (!voting && a.me_voted) {
        Stroke::new(1.0, t.accent)
    } else {
        Stroke::new(1.0, t.divider)
    };
    p.rect_stroke(rect, CornerRadius::same(8), border, StrokeKind::Inside);

    // Emoji, then the answer text.
    let mut x = rect.left() + 12.0;
    if let Some(e) = &a.emoji {
        let er = Rect::from_center_size(
            Pos2::new(x + EMOJI / 2.0, rect.center().y),
            Vec2::splat(EMOJI),
        );
        match (e.id, e.name.as_deref()) {
            (Some(id), _) => {
                let mut child = ui.new_child(UiBuilder::new().max_rect(er));
                media.borrow_mut().emoji(&mut child, id, e.animated, EMOJI);
            }
            (None, Some(name)) => {
                let g = emoji_text::layout(
                    ui.painter(),
                    name,
                    prop(EMOJI * 0.8),
                    t.strong,
                    f32::INFINITY,
                    1,
                );
                g.paint(
                    ui.painter(),
                    Pos2::new(er.left(), er.center().y - g.size().y / 2.0),
                    media,
                );
            }
            _ => {}
        }
        x += EMOJI + 10.0;
    }

    // Right side: radio / check box while voting, else percentage, count and my check.
    let right_w = if voting { 28.0 } else { 120.0 };
    let text_w = (rect.right() - right_w - 12.0 - x).max(20.0);
    let g = emoji_text::layout(ui.painter(), &a.text, prop(15.0), t.strong, text_w, 1);
    g.paint(
        ui.painter(),
        Pos2::new(x, rect.center().y - g.size().y / 2.0),
        media,
    );

    let p = ui.painter();
    if voting {
        let c = Pos2::new(rect.right() - 22.0, rect.center().y);
        let box_r = Rect::from_center_size(c, Vec2::splat(18.0));
        let edge = if selected { t.accent } else { t.muted };
        if multi {
            // Check box.
            if selected {
                p.rect_filled(box_r, CornerRadius::same(4), t.accent);
                paint_mark(p, c, 9.0, t.on_accent);
            } else {
                p.rect_stroke(
                    box_r,
                    CornerRadius::same(4),
                    Stroke::new(1.5, edge),
                    StrokeKind::Inside,
                );
            }
        } else {
            // Radio button.
            p.circle_stroke(c, 8.5, Stroke::new(1.5, edge));
            if selected {
                p.circle_filled(c, 5.0, t.accent);
            }
        }
    } else {
        let mut rx = rect.right() - 12.0;
        if a.me_voted {
            let c = Pos2::new(rx - 9.0, rect.center().y);
            paint_check(p, c, 9.0, t.accent, t.on_accent);
            rx -= 26.0;
        }
        let pct_g = p.layout_no_wrap(format!("{share}%"), bold(14.0), t.strong);
        let pw = pct_g.size().x;
        p.galley(
            Pos2::new(rx - pw, rect.center().y - pct_g.size().y / 2.0),
            pct_g,
            t.strong,
        );
        let cnt = p.layout_no_wrap(votes_label(a.count), prop(12.0), t.muted);
        let cw = cnt.size().x;
        p.galley(
            Pos2::new(rx - pw - 8.0 - cw, rect.center().y - cnt.size().y / 2.0),
            cnt,
            t.muted,
        );
    }
    voting
        && !busy
        && resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
}

/// A filled circle with a check mark.
fn paint_check(p: &egui::Painter, c: Pos2, r: f32, fill: Color32, mark: Color32) {
    p.circle_filled(c, r, fill);
    paint_mark(p, c, r, mark);
}

/// A check mark centred on `c`, about `r` wide on each side.
fn paint_mark(p: &egui::Painter, c: Pos2, r: f32, mark: Color32) {
    let s = r * 0.5;
    let pts = vec![
        c + Vec2::new(-s, 0.0),
        c + Vec2::new(-s * 0.3, s * 0.7),
        c + Vec2::new(s, -s * 0.6),
    ];
    p.add(egui::Shape::line(pts, Stroke::new(1.8, mark)));
}

/// The accent "Vote" button. Returns whether it was clicked.
fn vote_button(
    ui: &mut Ui,
    t: &Theme,
    message: Snowflake,
    r: Rect,
    label: &str,
    enabled: bool,
) -> bool {
    let resp = ui.interact(
        r,
        Id::unique(("nixcord-poll-vote", message.0)),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let fill = if !enabled {
        t.accent.gamma_multiply(0.45)
    } else if resp.hovered() {
        t.accent.gamma_multiply(0.85)
    } else {
        t.accent
    };
    ui.painter().rect_filled(r, CornerRadius::same(6), fill);
    ui.painter().text(
        r.center(),
        Align2::CENTER_CENTER,
        label,
        bold(14.0),
        if enabled {
            t.on_accent
        } else {
            t.on_accent.gamma_multiply(0.7)
        },
    );
    enabled
        && resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::Emoji;

    fn answer(id: u32, text: &str, count: u32, me: bool) -> PollAnswer {
        PollAnswer {
            id,
            text: text.into(),
            emoji: None,
            count,
            me_voted: me,
        }
    }

    fn poll(answers: Vec<PollAnswer>, expiry: Option<&str>, finalized: bool) -> Poll {
        Poll {
            question: "Which board game tonight?".into(),
            answers,
            expiry: expiry.map(Into::into),
            has_results: true,
            finalized,
            ..Default::default()
        }
    }

    const NOW: i64 = 1_900_000_000;

    fn iso(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn percentages_sum_to_100_and_handle_zero() {
        assert_eq!(percentages(&[0, 0, 0]), vec![0, 0, 0]);
        assert_eq!(percentages(&[]), Vec::<u32>::new());
        assert_eq!(percentages(&[1, 1, 1]), vec![34, 33, 33]);
        assert_eq!(percentages(&[2, 1]), vec![67, 33]);
        assert_eq!(percentages(&[1, 0]), vec![100, 0]);
        assert_eq!(percentages(&[1, 1, 1, 1, 1, 1, 1]).iter().sum::<u32>(), 100);
        assert_eq!(percentages(&[5, 5]), vec![50, 50]);
        for counts in [
            [7u32, 3, 1, 0],
            [1, 2, 3, 4],
            [999, 1, 0, 0],
            [u32::MAX, u32::MAX, 1, 0],
        ] {
            let p = percentages(&counts);
            assert_eq!(p.iter().sum::<u32>(), 100, "{counts:?}");
            assert_eq!(p[3] == 0, counts[3] == 0, "{counts:?}");
        }
    }

    #[test]
    fn expiry_and_footer_text() {
        assert_eq!(
            expiry_unix("2030-01-01T00:00:00.000000+00:00"),
            Some(1_893_456_000)
        );
        assert_eq!(expiry_unix("<str>"), None);
        assert_eq!(time_left(5 * 3600 + 120), "5h left");
        assert_eq!(time_left(2 * 86_400 + 5), "2d left");
        assert_eq!(time_left(12 * 60 + 59), "12m left");
        assert_eq!(time_left(30), "<1m left");
        assert_eq!(time_left(-5), "<1m left");

        let open = poll(
            vec![answer(1, "A", 2, false), answer(2, "B", 1, false)],
            Some(&iso(NOW + 5 * 3600 + 10)),
            false,
        );
        assert_eq!(footer_text(&open, NOW), "3 votes \u{b7} 5h left");
        assert!(!is_closed(&open, NOW));
        assert_eq!(
            footer_text(&open, NOW + 6 * 3600),
            "3 votes \u{b7} Poll closed"
        );
        assert!(is_closed(&open, NOW + 6 * 3600));

        let fin = poll(vec![answer(1, "A", 1, false)], None, true);
        assert_eq!(footer_text(&fin, NOW), "1 vote \u{b7} Poll closed");
        let forever = poll(vec![answer(1, "A", 0, false)], None, false);
        assert_eq!(footer_text(&forever, NOW), "0 votes");

        assert_eq!(next_change(5 * 3600 + 10), 10);
        assert_eq!(next_change(30), 30);
        assert_eq!(next_change(0), 1);
    }

    struct Harness {
        ctx: egui::Context,
        media: RefCell<Media>,
    }

    fn harness() -> Harness {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let prop_family = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("bold".into()), prop_family);
        ctx.set_fonts(fonts);
        let media = RefCell::new(Media::new(&ctx, true));
        Harness { ctx, media }
    }

    /// Run one frame with an optional click at `click`; returns the actions. Widget rects
    /// come from the previous frame (`rect_of`).
    fn frame(h: &Harness, p: &Poll, click: Option<Pos2>) -> Vec<Action> {
        let mut actions = Vec::new();
        let t = theme::by_name(theme::DEFAULT_THEME);
        let mut events = Vec::new();
        if let Some(pos) = click {
            events.push(egui::Event::PointerMoved(pos));
            events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            });
            events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            });
        }
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut out = h.ctx.run_ui(raw, |ui| {
            show_with(
                ui,
                &h.media,
                t,
                Snowflake(20),
                Snowflake(300),
                p,
                NOW,
                &mut actions,
            );
        });
        out.textures_delta.clear();
        actions
    }

    fn rect_of(h: &Harness, id: Id) -> Rect {
        h.ctx
            .read_response(id)
            .unwrap_or_else(|| panic!("no widget {id:?}"))
            .rect
    }

    fn open_poll(multi: bool) -> Poll {
        let mut p = poll(
            vec![
                PollAnswer {
                    emoji: Some(Emoji {
                        name: Some("\u{1F3B2}".into()),
                        ..Default::default()
                    }),
                    ..answer(1, "Dice tower", 2, false)
                },
                PollAnswer {
                    emoji: Some(Emoji {
                        id: Some(Snowflake(77)),
                        name: Some("meeple".into()),
                        animated: false,
                    }),
                    ..answer(2, "Worker placement", 1, false)
                },
                answer(3, "Something new", 0, false),
            ],
            Some(&iso(NOW + 3600 * 5 + 30)),
            false,
        );
        p.allow_multiselect = multi;
        p
    }

    #[test]
    fn open_poll_selects_and_votes() {
        let h = harness();
        let p = open_poll(false);
        assert!(frame(&h, &p, None).is_empty());
        let vote = Id::unique(("nixcord-poll-vote", 300u64));
        // Vote is disabled until something is selected.
        let vr = rect_of(&h, vote);
        assert!(frame(&h, &p, Some(vr.center())).is_empty());
        // Radio behaviour: picking 2 then 1 leaves only 1.
        for a in [2u32, 1] {
            let r = rect_of(&h, Id::unique(("nixcord-poll-answer", 300u64, a)));
            assert!(frame(&h, &p, Some(r.center())).is_empty());
        }
        frame(&h, &p, None);
        let acts = frame(&h, &p, Some(rect_of(&h, vote).center()));
        match acts.as_slice() {
            [Action::PollVote {
                channel,
                message,
                answer_ids,
            }] => {
                assert_eq!((*channel, *message), (Snowflake(20), Snowflake(300)));
                assert_eq!(answer_ids, &vec![1]);
            }
            other => panic!("{other:?}"),
        }
        // Now pending: a second click sends nothing.
        frame(&h, &p, None);
        assert!(frame(&h, &p, Some(rect_of(&h, vote).center())).is_empty());
    }

    #[test]
    fn multiselect_votes_for_several() {
        let h = harness();
        let p = open_poll(true);
        frame(&h, &p, None);
        for a in [3u32, 1] {
            let r = rect_of(&h, Id::unique(("nixcord-poll-answer", 300u64, a)));
            frame(&h, &p, Some(r.center()));
        }
        frame(&h, &p, None);
        let vote = Id::unique(("nixcord-poll-vote", 300u64));
        let acts = frame(&h, &p, Some(rect_of(&h, vote).center()));
        assert!(
            matches!(acts.as_slice(), [Action::PollVote { answer_ids, .. }] if answer_ids == &vec![1, 3]),
            "{acts:?}"
        );
    }

    #[test]
    fn voted_poll_shows_results_and_removes_vote() {
        let h = harness();
        let mut p = open_poll(false);
        p.answers[1].me_voted = true;
        assert!(frame(&h, &p, None).is_empty());
        // Results mode: answers are not clickable, there is no Vote button.
        let r = rect_of(&h, Id::unique(("nixcord-poll-answer", 300u64, 1u32)));
        assert!(frame(&h, &p, Some(r.center())).is_empty());
        assert!(h
            .ctx
            .read_response(Id::unique(("nixcord-poll-vote", 300u64)))
            .is_none());
        let remove = rect_of(&h, Id::unique(("nixcord-poll-remove", 300u64)));
        let acts = frame(&h, &p, Some(remove.center()));
        assert!(
            matches!(acts.as_slice(), [Action::PollVote { answer_ids, .. }] if answer_ids.is_empty()),
            "{acts:?}"
        );
    }

    #[test]
    fn closed_poll_has_no_controls() {
        let h = harness();
        let mut p = open_poll(false);
        p.finalized = true;
        p.answers[0].me_voted = true;
        assert!(frame(&h, &p, None).is_empty());
        let r = rect_of(&h, Id::unique(("nixcord-poll-answer", 300u64, 2u32)));
        assert!(frame(&h, &p, Some(r.center())).is_empty());
        assert!(h
            .ctx
            .read_response(Id::unique(("nixcord-poll-vote", 300u64)))
            .is_none());
        assert!(h
            .ctx
            .read_response(Id::unique(("nixcord-poll-remove", 300u64)))
            .is_none());
        // Expired by time, not finalized, also closes it.
        let mut late = open_poll(false);
        late.expiry = Some(iso(NOW - 1));
        assert!(frame(&h, &late, None).is_empty());
        assert!(h
            .ctx
            .read_response(Id::unique(("nixcord-poll-vote", 300u64)))
            .is_none());
    }
}

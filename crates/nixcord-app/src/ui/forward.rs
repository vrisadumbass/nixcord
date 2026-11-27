//! Forwarded messages (`message_reference.type == 1` with `message_snapshots`): a small
//! "Forwarded" label with an arrow, an indented block with a left bar holding the
//! snapshot's content (same markdown path as normal content), attachments, embeds and
//! stickers, and a dimmed source line. The source line names the original channel when
//! the store knows it and jumps to the original message when I can view that channel.
//!
//! A forward is never drawn as a reply: chat uses `Message::reply_target`, which skips
//! forwards.

use super::markdown::{self, MdClick, MentionInfo, RenderCtx, Resolver, StoreResolver};
use super::{prop, Action};
use crate::app::NixApp;
use crate::media::{Media, MediaEmoji};
use crate::theme::Theme;
use eframe::egui::{self, CornerRadius, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use nixcord_model::{ChannelKind, Message, SnapshotMessage, Snowflake};
use nixcord_store::Store;
use std::borrow::Cow;
use std::cell::RefCell;

/// Salt for the markdown cache key and widget ids of a snapshot (keyed by the outer id).
const SNAPSHOT_SALT: u64 = 0x5f0d_0000_0000_0000;
/// Embed index offset so snapshot embeds don't share ids with the outer message's.
const EMBED_INDEX_OFFSET: usize = 8;

/// Draw `m`'s forwarded snapshot, if it is a forward. Returns whether it mentions me.
pub fn show(ui: &mut Ui, app: &NixApp, m: &Message, actions: &mut Vec<Action>) -> bool {
    show_with(
        ui,
        Some(app),
        &app.store,
        &app.media,
        app.theme,
        app.prefs.clock_24h,
        m,
        actions,
    )
}

/// Rough height for the chat's row estimate.
pub fn estimate(m: &Message) -> f32 {
    if !m.is_forward() {
        return 0.0;
    }
    m.message_snapshots.first().map_or(0.0, |s| {
        let s = &s.message;
        let lines = 1.0 + s.content.matches('\n').count() as f32 + s.content.len() as f32 / 80.0;
        48.0 + lines * 21.0
            + s.attachments.len() as f32 * 304.0
            + s.embeds.len() as f32 * 160.0
            + s.sticker_items.len() as f32 * 164.0
    })
}

/// Where the forward came from: `(channel, message)` from `message_reference`.
pub fn source(m: &Message) -> Option<(Snowflake, Option<Snowflake>)> {
    let r = m.message_reference.as_ref()?;
    Some((r.channel_id?, r.message_id))
}

/// "#name" for guild channels, the plain label for DMs; `None` when the store doesn't
/// know the channel.
pub fn source_label(store: &Store, channel: Snowflake) -> Option<String> {
    let c = store.channel(channel)?;
    Some(match c.kind {
        ChannelKind::Dm | ChannelKind::GroupDm => c.label(),
        _ => format!("#{}", c.label()),
    })
}

/// The jump target, when I can view the source channel and know the message id.
pub fn jump_target(store: &Store, m: &Message) -> Option<Action> {
    let (channel_id, message_id) = source(m)?;
    let message_id = message_id?;
    store.can_view(channel_id).then_some(Action::JumpToMessage {
        channel_id,
        message_id,
    })
}

/// Snapshot mentions first, then the store (via the outer message's guild).
struct SnapshotResolver<'a> {
    inner: StoreResolver<'a>,
    snap: &'a SnapshotMessage,
}

impl Resolver for SnapshotResolver<'_> {
    fn user(&self, id: u64) -> Option<MentionInfo<'_>> {
        match self.snap.mentions.iter().find(|u| u.id.0 == id) {
            Some(u) => Some(MentionInfo {
                name: Cow::Borrowed(u.display_name()),
                color: None,
            }),
            None => self.inner.user(id),
        }
    }
    fn role(&self, id: u64) -> Option<MentionInfo<'_>> {
        self.inner.role(id)
    }
    fn channel(&self, id: u64) -> Option<MentionInfo<'_>> {
        self.inner.channel(id)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn show_with(
    ui: &mut Ui,
    app: Option<&NixApp>,
    store: &Store,
    media: &RefCell<Media>,
    t: &Theme,
    h24: bool,
    m: &Message,
    actions: &mut Vec<Action>,
) -> bool {
    if !m.is_forward() {
        return false;
    }
    let Some(snap) = m.message_snapshots.first().map(|s| &s.message) else {
        return false;
    };
    let mut mentions_me = false;

    // "↪ Forwarded"
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
        paint_forward_arrow(ui.painter(), r, t.muted);
        ui.label(
            egui::RichText::new("Forwarded")
                .font(prop(13.0))
                .italics()
                .color(t.muted),
        );
    });
    ui.add_space(-2.0);

    let resp = Frame::new()
        .inner_margin(Margin {
            left: 12,
            right: 0,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if !snap.content.is_empty() {
                let key = m.id.0 ^ SNAPSHOT_SALT;
                let doc = markdown::parse_cached(
                    ui.ctx(),
                    key,
                    snap.edited_timestamp.as_deref(),
                    &snap.content,
                );
                let resolver = SnapshotResolver {
                    inner: StoreResolver::new(store, Some(m)),
                    snap,
                };
                let emoji = MediaEmoji(media);
                let mut ctx = RenderCtx::new(t, &resolver, Id::unique(("md-fwd", m.id.0)));
                ctx.me = store.me().map(|u| u.id.0);
                ctx.edited = snap.edited_timestamp.is_some();
                ctx.h24 = h24;
                ctx.emoji = Some(&emoji);
                let out = markdown::render(ui, &*doc, &ctx);
                if let Some(a) = out.clicked.as_ref().and_then(MdClick::action) {
                    actions.push(a);
                }
                mentions_me = out.mentions_me;
            }
            if let Some(app) = app {
                for a in &snap.attachments {
                    super::chat::attachment(ui, app, a);
                }
                for (i, e) in snap.embeds.iter().enumerate() {
                    super::chat::embed_card(ui, app, m, i + EMBED_INDEX_OFFSET, e, actions);
                }
                for s in &snap.sticker_items {
                    super::chat::sticker(ui, app, s);
                }
            }
            source_line(ui, store, t, h24, m, snap, actions);
        });
    let r = resp.response.rect;
    ui.painter().rect_filled(
        Rect::from_min_size(r.min, Vec2::new(4.0, r.height())),
        CornerRadius::same(2),
        t.muted.gamma_multiply(0.45),
    );
    mentions_me
}

/// "#channel · Today at 4:05 PM", dimmed; clickable when the original can be opened.
fn source_line(
    ui: &mut Ui,
    store: &Store,
    t: &Theme,
    h24: bool,
    m: &Message,
    snap: &SnapshotMessage,
    actions: &mut Vec<Action>,
) {
    let label = source(m).and_then(|(c, _)| source_label(store, c));
    let stamp = chrono::DateTime::parse_from_rfc3339(&snap.timestamp)
        .ok()
        .and_then(|d| u64::try_from(d.timestamp_millis()).ok())
        .map(|ms| super::fmt_message_stamp(ms, h24));
    let text = match (&label, &stamp) {
        (Some(l), Some(s)) => format!("{l} \u{b7} {s}"),
        (Some(l), None) => l.clone(),
        (None, Some(s)) => s.clone(),
        (None, None) => return,
    };
    let jump = label.as_ref().and_then(|_| jump_target(store, m));
    let color = t.muted.gamma_multiply(0.8);
    let g = ui.painter().layout_no_wrap(text, prop(12.0), color);
    let (rect, _) = ui.allocate_exact_size(g.size(), Sense::hover());
    let resp = ui.interact(
        rect,
        Id::unique(("nixcord-forward-source", m.id.0)),
        if jump.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hot = jump.is_some() && resp.hovered();
    let paint_color = if hot { t.text } else { color };
    ui.painter().galley(rect.min, g, paint_color);
    if hot {
        ui.painter()
            .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, paint_color));
    }
    if let Some(a) = jump {
        if resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Jump to the original message")
            .clicked()
        {
            actions.push(a);
        }
    }
}

/// A small curved arrow pointing right (the forward glyph).
fn paint_forward_arrow(p: &egui::Painter, r: Rect, color: egui::Color32) {
    let c = r.center();
    let s = r.width() * 0.32;
    let stroke = Stroke::new(1.6, color);
    // Hook: down-left to the shaft, then right to the head.
    let pts: Vec<Pos2> = vec![
        c + Vec2::new(-s, s),
        c + Vec2::new(-s, 0.2 * s),
        c + Vec2::new(-0.7 * s, -0.2 * s),
        c + Vec2::new(-0.2 * s, -0.3 * s),
        c + Vec2::new(s, -0.3 * s),
    ];
    p.add(egui::Shape::line(pts, stroke));
    p.line_segment(
        [c + Vec2::new(s, -0.3 * s), c + Vec2::new(0.5 * s, -0.8 * s)],
        stroke,
    );
    p.line_segment(
        [c + Vec2::new(s, -0.3 * s), c + Vec2::new(0.5 * s, 0.2 * s)],
        stroke,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{
        permissions::VIEW_CHANNEL, Channel, GatewayEvent, Guild, MessageReference, MessageSnapshot,
        Overwrite, OverwriteKind, Ready, Role, User,
    };

    const G: Snowflake = Snowflake(10);
    const OPEN: Snowflake = Snowflake(102);
    const HIDDEN: Snowflake = Snowflake(109);

    fn store() -> Store {
        let me = User {
            id: Snowflake(1000),
            username: "you".into(),
            ..Default::default()
        };
        let mut s = Store::new();
        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me.clone(),
            guilds: vec![Guild {
                id: G,
                name: "Workshop".into(),
                roles: vec![Role {
                    id: G,
                    name: "@everyone".into(),
                    permissions: VIEW_CHANNEL,
                    ..Default::default()
                }],
                channels: vec![
                    Channel {
                        id: OPEN,
                        guild_id: Some(G),
                        name: Some("lounge".into()),
                        ..Default::default()
                    },
                    Channel {
                        id: HIDDEN,
                        guild_id: Some(G),
                        name: Some("staff".into()),
                        permission_overwrites: vec![Overwrite {
                            id: G,
                            kind: OverwriteKind::Role,
                            allow: 0,
                            deny: VIEW_CHANNEL,
                        }],
                        ..Default::default()
                    },
                ],
                members: vec![nixcord_model::GuildMember {
                    user: Some(me),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            session_id: "s".into(),
            ..Default::default()
        })));
        s
    }

    fn forward(from: Snowflake) -> Message {
        Message {
            id: Snowflake(800),
            channel_id: OPEN,
            guild_id: Some(G),
            author: User {
                id: Snowflake(1005),
                username: "pip".into(),
                ..Default::default()
            },
            timestamp: "2026-11-23T08:00:00Z".into(),
            message_reference: Some(MessageReference {
                message_id: Some(Snowflake(700)),
                channel_id: Some(from),
                guild_id: Some(G),
                kind: 1,
            }),
            message_snapshots: vec![MessageSnapshot {
                message: SnapshotMessage {
                    content: "Kettle's on, **come grab a mug** <@1000>".into(),
                    timestamp: "2026-11-23T07:55:00Z".into(),
                    ..Default::default()
                },
            }],
            ..Default::default()
        }
    }

    fn context() -> egui::Context {
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
        ctx
    }

    /// One frame on `ctx`, with an optional click. Returns (actions, mentions me).
    fn run(
        ctx: &egui::Context,
        store: &Store,
        m: &Message,
        click: Option<Pos2>,
    ) -> (Vec<Action>, bool) {
        let media = RefCell::new(Media::new(ctx, true));
        let t = theme::by_name(theme::DEFAULT_THEME);
        let mut events = Vec::new();
        if let Some(pos) = click {
            events.push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                });
            }
        }
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut actions = Vec::new();
        let mut mentions = false;
        let mut out = ctx.run_ui(raw, |ui| {
            mentions = show_with(ui, None, store, &media, t, false, m, &mut actions);
        });
        out.textures_delta.clear();
        (actions, mentions)
    }

    #[test]
    fn forward_renders_and_jumps_to_original() {
        let s = store();
        let ctx = context();
        let m = forward(OPEN);
        assert!(m.reply_target().is_none());
        assert_eq!(source_label(&s, OPEN).as_deref(), Some("#lounge"));
        let (acts, mentions) = run(&ctx, &s, &m, None);
        assert!(acts.is_empty());
        assert!(mentions, "the snapshot mentions me");
        let src = ctx
            .read_response(Id::unique(("nixcord-forward-source", 800u64)))
            .expect("source line")
            .rect;
        let (acts, _) = run(&ctx, &s, &m, Some(src.center()));
        assert!(
            matches!(
                acts.as_slice(),
                [Action::JumpToMessage { channel_id, message_id }]
                    if *channel_id == OPEN && *message_id == Snowflake(700)
            ),
            "{acts:?}"
        );
    }

    #[test]
    fn forward_from_hidden_or_unknown_channel_does_not_jump() {
        let s = store();
        let ctx = context();
        let hidden = forward(HIDDEN);
        assert!(!s.can_view(HIDDEN));
        assert!(jump_target(&s, &hidden).is_none());
        run(&ctx, &s, &hidden, None);
        let src = ctx
            .read_response(Id::unique(("nixcord-forward-source", 800u64)))
            .unwrap()
            .rect;
        let (acts, _) = run(&ctx, &s, &hidden, Some(src.center()));
        assert!(acts.is_empty());

        let unknown = forward(Snowflake(4242));
        assert_eq!(source_label(&s, Snowflake(4242)), None);
        assert!(jump_target(&s, &unknown).is_none());
        let (acts, _) = run(&ctx, &s, &unknown, None);
        assert!(acts.is_empty());
    }

    #[test]
    fn plain_messages_and_replies_draw_nothing() {
        let s = store();
        let ctx = context();
        let mut reply = forward(OPEN);
        reply.message_reference.as_mut().unwrap().kind = 0;
        assert_eq!(estimate(&reply), 0.0);
        let (acts, mentions) = run(&ctx, &s, &reply, None);
        assert!(acts.is_empty() && !mentions);
        assert!(ctx
            .read_response(Id::unique(("nixcord-forward-source", 800u64)))
            .is_none());
        assert!(estimate(&forward(OPEN)) > 0.0);
    }
}

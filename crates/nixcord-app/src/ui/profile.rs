//! Discord-style floating user profile popout card.
//!
//! Triggered by clicking an author avatar/name in chat, a member-list row, a DM header
//! recipient, or right-clicking a DM sidebar row. Dismissed on Escape or clicking outside.

use super::{bold, emoji_text, id_color, paint_app_badge, presence, prop, Action};
use crate::app::NixApp;
use crate::theme::Theme;
use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Frame, Id, Margin, Pos2, Rect, RichText, Sense,
    Stroke, Ui, Vec2,
};
use nixcord_model::{Activity, ActivityKind, RelationshipKind, Snowflake};
use nixcord_store::{Store, UserProfile};

const WIDTH: f32 = 300.0;
const BANNER_H: f32 = 60.0;
const AVATAR_D: f32 = 64.0;
const MARGIN: f32 = 8.0;

/// Open profile popout target (stored in `ViewState`, cleared on logout / dismissal).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfilePopout {
    pub user_id: Snowflake,
    pub guild_id: Option<Snowflake>,
    pub anchor: Rect,
}

fn height_id() -> Id {
    Id::unique("nixcord-profile-popout-h")
}

fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Resolve profile via `Store::user_profile` (with a fallback from individual store
/// accessors so popouts work even against partial store implementations).
fn resolve_profile<'a>(
    store: &'a Store,
    user_id: Snowflake,
    guild_id: Option<Snowflake>,
) -> Option<UserProfile<'a>> {
    if let Some(p) = store.user_profile(user_id, guild_id) {
        return Some(p);
    }
    let user = store.user(user_id)?;
    let nick = guild_id.and_then(|g| store.member_nick(g, user_id));
    let member_avatar = guild_id.and_then(|g| store.member_avatar(g, user_id));
    let roles = guild_id
        .map(|g| store.member_roles(g, user_id))
        .unwrap_or_default();
    let color = guild_id.and_then(|g| store.member_color(g, user_id));
    let status = store.status(user_id);
    let presence = store.presence(user_id);
    let relationship = store
        .relationship(user_id)
        .map_or(RelationshipKind::None, |r| r.kind);
    let is_me = store.me().is_some_and(|m| m.id == user_id);
    Some(UserProfile {
        user,
        nick,
        member_avatar,
        roles,
        color,
        status,
        presence,
        relationship,
        is_me,
    })
}

/// Choose a screen-clamped top-left position for the popout next to `anchor`:
/// opens to the left when anchored on the right half of the window (member list),
/// otherwise to the right (chat / sidebar).
pub(crate) fn place_popout(screen: Rect, anchor: Rect, est_h: f32) -> Pos2 {
    let open_left = anchor.center().x > screen.center().x;
    let raw_x = if open_left {
        anchor.left() - WIDTH - 8.0
    } else {
        anchor.right() + 8.0
    };
    let max_x = (screen.right() - WIDTH - MARGIN).max(screen.left() + MARGIN);
    let x = raw_x.clamp(screen.left() + MARGIN, max_x);

    let max_y = (screen.bottom() - est_h - MARGIN).max(screen.top() + MARGIN);
    let y = anchor.top().clamp(screen.top() + MARGIN, max_y);
    Pos2::new(x, y)
}

/// Draw the floating profile card when `app.view.profile_popout` is `Some`.
pub fn show(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    let Some(popout) = app.view.profile_popout else {
        return;
    };
    let Some(profile) = resolve_profile(&app.store, popout.user_id, popout.guild_id) else {
        app.view.profile_popout = None;
        return;
    };

    let t = app.theme;
    let card_bg = t.elevated.to_opaque();
    let banner_color = profile
        .color
        .filter(|&c| c != 0)
        .map(rgb)
        .unwrap_or_else(|| id_color(profile.user.id));

    let est_h: f32 = ctx.data(|d| d.get_temp(height_id())).unwrap_or(280.0);
    let pos = place_popout(ctx.content_rect(), popout.anchor, est_h);

    let user = profile.user.clone();
    let display_primary = profile
        .nick
        .unwrap_or_else(|| profile.user.display_name())
        .to_string();
    let global_differs = profile.nick.is_some()
        && profile.user.global_name.as_deref() != Some(display_primary.as_str());
    let guild_avatar = popout
        .guild_id
        .map(|g| (g, profile.member_avatar.map(str::to_owned)));
    let status = profile.status;
    let streaming = profile
        .presence
        .is_some_and(nixcord_model::Presence::is_streaming);
    let relationship = profile.relationship;
    let is_me = profile.is_me;

    let custom_status: Option<String> = profile.presence.and_then(|p| {
        p.activities
            .iter()
            .find(|a| a.kind == ActivityKind::Custom)
            .and_then(Activity::label)
    });
    let rich_activity: Option<Activity> = profile.presence.and_then(|p| {
        p.activities
            .iter()
            .find(|a| a.kind != ActivityKind::Custom && a.label().is_some())
            .cloned()
    });
    let roles: Vec<(String, u32)> = profile
        .roles
        .iter()
        .filter(|r| r.name != "@everyone")
        .map(|r| (r.name.clone(), r.color))
        .collect();

    let mut close = false;
    let area = egui::Area::new(Id::unique("nixcord-profile-popout"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            Frame::new()
                .fill(card_bg)
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(120),
                })
                .inner_margin(Margin::ZERO)
                .show(ui, |ui| {
                    ui.set_width(WIDTH);
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

                    // Top banner.
                    let (banner_rect, _) =
                        ui.allocate_exact_size(Vec2::new(WIDTH, BANNER_H), Sense::hover());
                    ui.painter().rect_filled(
                        banner_rect,
                        CornerRadius {
                            nw: 10,
                            ne: 10,
                            sw: 0,
                            se: 0,
                        },
                        banner_color,
                    );

                    // Overlapping avatar row: reserves vertical space below the banner for
                    // the lower half of the 64px avatar + right-aligned relationship badge.
                    let (avatar_row, _) = ui.allocate_exact_size(
                        Vec2::new(WIDTH, AVATAR_D * 0.5 + 8.0),
                        Sense::hover(),
                    );
                    let avatar_center = Pos2::new(
                        banner_rect.left() + 16.0 + AVATAR_D / 2.0,
                        banner_rect.bottom(),
                    );
                    ui.painter()
                        .circle_filled(avatar_center, AVATAR_D / 2.0 + 4.5, card_bg);
                    let member_av = guild_avatar.as_ref().map(|(g, h)| (*g, h.as_deref()));
                    app.media.borrow_mut().paint_avatar(
                        ui,
                        avatar_center,
                        AVATAR_D,
                        &user,
                        member_av,
                    );
                    presence::paint_status(
                        ui,
                        t,
                        avatar_center,
                        AVATAR_D,
                        card_bg,
                        status,
                        streaming,
                    );

                    // Relationship badge (Friend / Blocked) in the top-right beside the avatar.
                    if let Some((badge_text, dot_col)) = relationship_badge(relationship, t) {
                        let g = ui
                            .painter()
                            .layout_no_wrap(badge_text.into(), bold(11.5), t.text);
                        let pill_w = g.size().x + 22.0;
                        let pill_rect = Rect::from_min_size(
                            Pos2::new(avatar_row.right() - 14.0 - pill_w, avatar_row.top() + 8.0),
                            Vec2::new(pill_w, 22.0),
                        );
                        ui.painter().rect(
                            pill_rect,
                            CornerRadius::same(11),
                            t.hover,
                            Stroke::new(1.0, t.divider),
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().circle_filled(
                            Pos2::new(pill_rect.left() + 10.0, pill_rect.center().y),
                            3.5,
                            dot_col,
                        );
                        ui.painter().galley(
                            Pos2::new(
                                pill_rect.left() + 17.0,
                                pill_rect.center().y - g.size().y / 2.0,
                            ),
                            g,
                            t.text,
                        );
                    }

                    // Padded body content.
                    Frame::new()
                        .inner_margin(Margin {
                            left: 14,
                            right: 14,
                            top: 2,
                            bottom: 14,
                        })
                        .show(ui, |ui| {
                            ui.set_width(WIDTH - 28.0);
                            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

                            // Primary name + APP badge.
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                emoji_text::label_truncated(
                                    ui,
                                    &app.media,
                                    &display_primary,
                                    bold(18.0),
                                    t.strong,
                                );
                                if user.bot {
                                    let (r, _) = ui
                                        .allocate_exact_size(Vec2::new(30.0, 16.0), Sense::hover());
                                    paint_app_badge(ui, r.left_center());
                                }
                            });

                            // @username (and global display name if a guild nick overrides it).
                            let handle = if global_differs {
                                format!("@{} · {}", user.username, user.display_name())
                            } else {
                                format!("@{}", user.username)
                            };
                            ui.label(RichText::new(handle).font(prop(13.0)).color(t.muted));

                            // Custom status.
                            if let Some(cs) = &custom_status {
                                ui.add_space(2.0);
                                emoji_text::label(ui, &app.media, cs, prop(13.5), t.text);
                            }

                            // Rich activity block.
                            if let Some(act) = &rich_activity {
                                section_divider(ui, t);
                                activity_card(ui, t, act);
                            }

                            // Roles section.
                            if !roles.is_empty() {
                                section_divider(ui, t);
                                ui.label(RichText::new("ROLES").font(bold(11.0)).color(t.muted));
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(5.0, 5.0);
                                    for (name, color) in &roles {
                                        role_pill(ui, t, name, *color);
                                    }
                                });
                            }

                            // Action buttons.
                            section_divider(ui, t);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                                if !is_me && action_btn(ui, t, "Message", true).clicked() {
                                    actions.push(Action::OpenDm(user.id));
                                    close = true;
                                }
                                if action_btn(ui, t, "Copy Username", false).clicked() {
                                    actions.push(Action::Copy(user.username.clone()));
                                    actions.push(Action::Toast("Copied username".into()));
                                }
                                if action_btn(ui, t, "Copy ID", false).clicked() {
                                    actions.push(Action::Copy(user.id.to_string()));
                                    actions.push(Action::Toast("Copied user ID".into()));
                                }
                            });
                        });
                });
        });

    let rect = area.response.rect;
    ctx.data_mut(|d| d.insert_temp(height_id(), rect.height()));

    let dismissed = ctx.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !rect.contains(p) && !popout.anchor.contains(p)))
    });
    if close || dismissed {
        app.view.profile_popout = None;
    }
}

fn relationship_badge(kind: RelationshipKind, t: &Theme) -> Option<(&'static str, Color32)> {
    match kind {
        RelationshipKind::Friend => Some(("Friend", t.online)),
        RelationshipKind::Blocked => Some(("Blocked", t.danger)),
        RelationshipKind::IncomingRequest => Some(("Pending Request", t.idle)),
        RelationshipKind::OutgoingRequest => Some(("Request Sent", t.idle)),
        RelationshipKind::None | RelationshipKind::Implicit | RelationshipKind::Other(_) => None,
    }
}

fn section_divider(ui: &mut Ui, t: &Theme) {
    ui.add_space(2.0);
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, t.divider);
    ui.add_space(2.0);
}

fn activity_heading(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Playing => "PLAYING A GAME",
        ActivityKind::Streaming => "LIVE ON STREAM",
        ActivityKind::Listening => "LISTENING TO",
        ActivityKind::Watching => "WATCHING",
        ActivityKind::Competing => "COMPETING IN",
        ActivityKind::Hang => "HANGING OUT",
        ActivityKind::Custom | ActivityKind::Other(_) => "ACTIVITY",
    }
}

fn activity_card(ui: &mut Ui, t: &Theme, act: &Activity) {
    Frame::new()
        .fill(t.hover)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new(activity_heading(act.kind))
                    .font(bold(10.5))
                    .color(t.muted),
            );
            if !act.name.trim().is_empty() {
                ui.label(RichText::new(&act.name).font(bold(13.5)).color(t.strong));
            }
            if let Some(d) = act.details.as_deref().filter(|s| !s.trim().is_empty()) {
                ui.add(egui::Label::new(RichText::new(d).font(prop(12.5)).color(t.text)).wrap());
            }
            if let Some(s) = act.state.as_deref().filter(|s| !s.trim().is_empty()) {
                ui.add(egui::Label::new(RichText::new(s).font(prop(12.5)).color(t.muted)).wrap());
            }
        });
}

fn role_pill(ui: &mut Ui, t: &Theme, name: &str, color: u32) {
    let dot = if color != 0 { rgb(color) } else { t.muted };
    let g = ui
        .painter()
        .layout_no_wrap(name.to_string(), prop(12.0), t.text);
    let size = Vec2::new(8.0 + 8.0 + 5.0 + g.size().x + 8.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(4),
        t.hover,
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .circle_filled(Pos2::new(rect.left() + 10.0, rect.center().y), 4.0, dot);
    ui.painter().galley(
        Pos2::new(rect.left() + 19.0, rect.center().y - g.size().y / 2.0),
        g,
        t.text,
    );
}

fn action_btn(ui: &mut Ui, t: &Theme, label: &str, primary: bool) -> egui::Response {
    let fg = if primary { t.on_accent } else { t.strong };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), bold(12.5), fg);
    let size = Vec2::new(g.size().x + 18.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if primary {
        if resp.hovered() {
            t.accent.gamma_multiply(0.88)
        } else {
            t.accent
        }
    } else if resp.hovered() {
        t.selected
    } else {
        t.hover
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        if primary {
            Stroke::NONE
        } else {
            Stroke::new(1.0, t.divider)
        },
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_popout_opens_left_from_member_list_and_clamps() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0));
        // Member list row on the right edge.
        let right_anchor = Rect::from_min_size(Pos2::new(1050.0, 620.0), Vec2::new(220.0, 40.0));
        let p = place_popout(screen, right_anchor, 320.0);
        assert!(p.x + WIDTH <= right_anchor.left());
        assert!(p.y + 320.0 <= screen.bottom() - MARGIN + 0.01);

        // Chat avatar near the left edge.
        let left_anchor = Rect::from_min_size(Pos2::new(330.0, 120.0), Vec2::new(40.0, 40.0));
        let p2 = place_popout(screen, left_anchor, 280.0);
        assert!(p2.x >= left_anchor.right());
        assert_eq!(p2.y, 120.0);
    }

    #[test]
    fn resolve_profile_falls_back_to_store_user() {
        use nixcord_model::{GatewayEvent, Guild, Ready, Role, User};

        let mut store = Store::new();
        let me = User {
            id: Snowflake(1),
            username: "me".into(),
            global_name: Some("Me".into()),
            ..Default::default()
        };
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me.clone(),
            guilds: vec![Guild {
                id: Snowflake(10),
                name: "Test".into(),
                roles: vec![Role {
                    id: Snowflake(11),
                    name: "dev".into(),
                    color: 0x3498DB,
                    position: 2,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            session_id: "s".into(),
            ..Default::default()
        })));
        let p = resolve_profile(&store, Snowflake(1), Some(Snowflake(10))).unwrap();
        assert_eq!(p.user.username, "me");
        assert!(p.is_me);
    }
}

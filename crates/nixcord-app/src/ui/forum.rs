//! Forum channel post board and Create Thread / New Post modal.
//!
//! Renders a Discord-style Forum view when a `ChannelKind::Forum` channel is open:
//! - Top header bar (`#forum`, topic, member-list toggle)
//! - Search-posts input, clickable tag filter pills (`Channel.available_tags`), and "+ New Post"
//! - Scrollable post cards (`Store::channel_threads`) with applied tags, unread/mention badges,
//!   cached recent-message preview, and click-to-open (`Action::SelectChannel`)
//!
//! Also renders the shared `create_modal` for creating either a new Forum post (title + tags +
//! required starter message) or a public thread in a regular text/announcement channel (title +
//! optional starter message).

use super::{
    bold, emoji_text, fmt_stamp, hline, icon_button, member_name, paint_search, prop, role_color,
    Action, Icon,
};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, Sense, Stroke, Ui, UiBuilder, Vec2,
};
use nixcord_model::{Channel, ForumTag, Message, Snowflake};
use nixcord_store::Store;
use std::cell::RefCell;
use std::collections::HashSet;

pub const MAX_THREAD_NAME_CHARS: usize = 100;
pub const MAX_STARTER_MSG_CHARS: usize = 2000;
pub const MAX_FORUM_TAGS: usize = 5;
const PREVIEW_MAX_CHARS: usize = 140;
const CONTENT_MAX_W: f32 = 920.0;

#[derive(Debug, Default)]
pub struct ForumState {
    pub forum_id: Option<Snowflake>,
    pub search: String,
    pub selected_tags: HashSet<Snowflake>,
}

impl ForumState {
    /// Reset search query and tag filters when switching to a different forum channel.
    pub fn sync_forum(&mut self, forum_id: Snowflake) {
        if self.forum_id != Some(forum_id) {
            self.forum_id = Some(forum_id);
            self.search.clear();
            self.selected_tags.clear();
        }
    }

    pub fn has_filters(&self) -> bool {
        !self.search.trim().is_empty() || !self.selected_tags.is_empty()
    }

    pub fn clear_filters(&mut self) {
        self.search.clear();
        self.selected_tags.clear();
    }
}

#[derive(Debug, Clone)]
pub struct CreateThreadModal {
    pub parent_id: Snowflake,
    pub is_forum: bool,
    pub title: String,
    pub message: String,
    pub selected_tags: Vec<Snowflake>,
    pub busy: bool,
    pub error: Option<String>,
    pub focus_title: bool,
}

impl CreateThreadModal {
    pub fn new(parent_id: Snowflake, is_forum: bool) -> Self {
        Self {
            parent_id,
            is_forum,
            title: String::new(),
            message: String::new(),
            selected_tags: Vec::new(),
            busy: false,
            error: None,
            focus_title: true,
        }
    }
}

/// Pure snapshot of a cached recent message for a forum post card.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PostPreview {
    author_name: String,
    author_color: Option<Color32>,
    snippet: String,
    timestamp_ms: u64,
}

/// Owned render data for one forum post card.
#[derive(Debug, Clone)]
struct PostCard {
    id: Snowflake,
    title: String,
    locked: bool,
    unread: bool,
    mentions: u32,
    tags: Vec<String>,
    preview: Option<PostPreview>,
    activity_ms: u64,
}

/// Inputs for rendering the forum board (decoupled from `NixApp` for headless testing).
struct ForumBoardData<'a> {
    forum_id: Snowflake,
    name: &'a str,
    topic: Option<&'a str>,
    available_tags: &'a [ForumTag],
    can_post: bool,
    total_posts: usize,
    cards: &'a [PostCard],
    clock_24h: bool,
    members_shown: bool,
}

// ---- pure helpers -------------------------------------------------------------------

/// Display label for a forum tag: `"<unicode emoji> <name>"` or plain `"<name>"`.
pub(crate) fn tag_display_text(tag: &ForumTag) -> String {
    match tag
        .emoji_name
        .as_deref()
        .filter(|e| !e.trim().is_empty() && tag.emoji_id.is_none())
    {
        Some(emoji) => format!("{emoji} {}", tag.name),
        None => tag.name.clone(),
    }
}

/// True when `thread` satisfies both `selected_tags` (AND semantics) and `query`
/// (case-insensitive substring over title, applied tag names, or cached message content/author).
pub(crate) fn post_matches_filters(
    thread: &Channel,
    available_tags: &[ForumTag],
    cached_messages: &[Message],
    query: &str,
    selected_tags: &HashSet<Snowflake>,
) -> bool {
    if !selected_tags.is_empty()
        && !selected_tags
            .iter()
            .all(|tag_id| thread.applied_tags.contains(tag_id))
    {
        return false;
    }
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    if thread.label().to_lowercase().contains(&needle) {
        return true;
    }
    for tag_id in &thread.applied_tags {
        if let Some(tag) = available_tags.iter().find(|t| t.id == *tag_id) {
            if tag.name.to_lowercase().contains(&needle) {
                return true;
            }
        }
    }
    cached_messages.iter().any(|m| {
        m.content.to_lowercase().contains(&needle)
            || m.author.display_name().to_lowercase().contains(&needle)
            || m.author.username.to_lowercase().contains(&needle)
    })
}

/// Filter `threads` by active tag pills and search query.
pub(crate) fn filter_posts<'a>(
    threads: &[&'a Channel],
    available_tags: &[ForumTag],
    store: &Store,
    query: &str,
    selected_tags: &HashSet<Snowflake>,
) -> Vec<&'a Channel> {
    threads
        .iter()
        .copied()
        .filter(|th| {
            post_matches_filters(
                th,
                available_tags,
                store.messages(th.id),
                query,
                selected_tags,
            )
        })
        .collect()
}

/// Compact single-line preview of a cached message (falls back to attachment/embed labels).
pub(crate) fn single_line_preview(msg: &Message) -> String {
    let collapsed: String = msg.content.split_whitespace().collect::<Vec<_>>().join(" ");
    if !collapsed.is_empty() {
        return match collapsed.char_indices().nth(PREVIEW_MAX_CHARS) {
            Some((idx, _)) => format!("{}…", &collapsed[..idx]),
            None => collapsed,
        };
    }
    if !msg.attachments.is_empty() {
        if msg.attachments.len() == 1 {
            return format!("[Attachment: {}]", msg.attachments[0].filename);
        }
        return format!("[{} attachments]", msg.attachments.len());
    }
    if let Some(embed) = msg.embeds.first() {
        if let Some(title) = embed.title.as_deref().filter(|s| !s.trim().is_empty()) {
            return format!("[Embed: {title}]");
        }
        return "[Embed]".into();
    }
    "[Message]".into()
}

/// Validate whether the Create Thread / New Forum Post modal can be submitted.
pub(crate) fn can_submit_create_thread(modal: &CreateThreadModal) -> bool {
    if modal.busy {
        return false;
    }
    let title_chars = modal.title.trim().chars().count();
    if title_chars == 0 || title_chars > MAX_THREAD_NAME_CHARS {
        return false;
    }
    let msg_chars = modal.message.trim().chars().count();
    if modal.is_forum {
        (1..=MAX_STARTER_MSG_CHARS).contains(&msg_chars)
    } else {
        msg_chars <= MAX_STARTER_MSG_CHARS
    }
}

fn badge_text(count: u32) -> String {
    if count > 99 {
        "99+".into()
    } else {
        count.to_string()
    }
}

// ---- forum view ---------------------------------------------------------------------

pub fn show(ui: &mut Ui, app: &mut NixApp, forum_id: Snowflake, actions: &mut Vec<Action>) {
    app.view.forum.sync_forum(forum_id);

    let Some(forum) = app.store.channel(forum_id) else {
        return;
    };
    let name = forum.label();
    let topic = forum.topic.clone().filter(|s| !s.trim().is_empty());
    let available_tags = forum.available_tags.clone();
    let guild_id = forum.guild_id;
    let can_post = app.store.can_send(forum_id) && !app.view.forbidden.contains(&forum_id);

    let all_threads = app.store.channel_threads(forum_id);
    let total_posts = all_threads.len();
    let filtered = filter_posts(
        &all_threads,
        &available_tags,
        &app.store,
        &app.view.forum.search,
        &app.view.forum.selected_tags,
    );
    let cards: Vec<PostCard> = filtered
        .into_iter()
        .map(|th| {
            let mentions = app.store.channel_mention_count(th.id);
            let unread = app.is_unread(th) || mentions > 0;
            let locked = th.thread_metadata.as_ref().is_some_and(|m| m.locked);
            let tags = th
                .applied_tags
                .iter()
                .filter_map(|tid| available_tags.iter().find(|t| t.id == *tid))
                .map(tag_display_text)
                .collect();
            let preview = app.store.messages(th.id).last().map(|m| PostPreview {
                author_name: member_name(&app.store, guild_id, &m.author).to_string(),
                author_color: role_color(&app.store, guild_id, m.author.id),
                snippet: single_line_preview(m),
                timestamp_ms: m.id.timestamp_ms(),
            });
            let activity_ms = th.last_message_id.unwrap_or(th.id).timestamp_ms();
            PostCard {
                id: th.id,
                title: th.label(),
                locked,
                unread,
                mentions,
                tags,
                preview,
                activity_ms,
            }
        })
        .collect();

    let data = ForumBoardData {
        forum_id,
        name: &name,
        topic: topic.as_deref(),
        available_tags: &available_tags,
        can_post,
        total_posts,
        cards: &cards,
        clock_24h: app.prefs.clock_24h,
        members_shown: app.members_visible(ui.ctx()),
    };
    let toggle_members = render_forum_board(
        ui,
        app.theme,
        &app.media,
        &data,
        &mut app.view.forum,
        actions,
    );
    if toggle_members {
        app.prefs.show_members = Some(!data.members_shown);
    }
}

/// Render the Forum top header and scrollable post board. Returns `true` if the user
/// clicked the member-list toggle button.
fn render_forum_board(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    data: &ForumBoardData<'_>,
    state: &mut ForumState,
    actions: &mut Vec<Action>,
) -> bool {
    let mut toggle_members = false;

    egui::Panel::top("forum_header")
        .exact_size(48.0)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(t.header)
                .inner_margin(Margin::symmetric(16, 0)),
        )
        .show(ui, |ui| {
            let full = ui.max_rect().expand2(Vec2::new(16.0, 0.0));
            hline(ui, full, full.bottom() - 0.5, t.divider);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let members = icon_button(
                    ui,
                    t,
                    Icon::Members,
                    32.0,
                    if data.members_shown {
                        "Hide Member List"
                    } else {
                        "Show Member List"
                    },
                );
                if data.members_shown {
                    ui.painter().rect_filled(
                        Rect::from_center_size(
                            members.rect.center_bottom() + Vec2::new(0.0, -3.0),
                            Vec2::new(16.0, 2.0),
                        ),
                        CornerRadius::same(1),
                        t.strong,
                    );
                }
                if members.clicked() {
                    toggle_members = true;
                }

                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new("💬").font(prop(16.0)).color(t.muted));
                    emoji_text::label_truncated(ui, media, data.name, bold(16.0), t.strong);
                    if let Some(topic) = data.topic {
                        ui.add_space(4.0);
                        let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 24.0), Sense::hover());
                        ui.painter().rect_filled(r, 0.0, t.divider);
                        ui.add_space(4.0);
                        let one_line = topic.lines().next().unwrap_or_default().to_string();
                        ui.add(
                            egui::Label::new(
                                RichText::new(one_line).font(prop(14.0)).color(t.muted),
                            )
                            .truncate(),
                        )
                        .on_hover_text(topic);
                    }
                });
            });
        });

    egui::CentralPanel::no_frame()
        .frame(Frame::new().fill(t.chat))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("forum_posts", data.forum_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    Frame::new()
                        .inner_margin(Margin::symmetric(24, 16))
                        .show(ui, |ui| {
                            let content_w = ui.available_width().clamp(260.0, CONTENT_MAX_W);
                            ui.set_max_width(content_w);

                            // Top toolbar: search box + "+ New Post" button.
                            toolbar(ui, t, data, state, actions);
                            ui.add_space(10.0);

                            // Clickable tag filter pills.
                            if !data.available_tags.is_empty() || state.has_filters() {
                                tag_filter_bar(ui, t, media, data.available_tags, state);
                                ui.add_space(10.0);
                            }

                            // Post count status line.
                            let count_label = if state.has_filters() {
                                format!(
                                    "{} of {} {}",
                                    data.cards.len(),
                                    data.total_posts,
                                    if data.total_posts == 1 {
                                        "post"
                                    } else {
                                        "posts"
                                    }
                                )
                            } else {
                                format!(
                                    "{} {}",
                                    data.total_posts,
                                    if data.total_posts == 1 {
                                        "post"
                                    } else {
                                        "posts"
                                    }
                                )
                            };
                            ui.label(RichText::new(count_label).font(bold(12.0)).color(t.muted));
                            ui.add_space(8.0);

                            if data.total_posts == 0 {
                                empty_forum_card(ui, t, data.forum_id, data.can_post, actions);
                            } else if data.cards.is_empty() {
                                empty_filtered_card(ui, t, state);
                            } else {
                                for card in data.cards {
                                    post_card(ui, t, media, card, data.clock_24h, actions);
                                    ui.add_space(8.0);
                                }
                            }
                        });
                });
        });

    toggle_members
}

fn toolbar(
    ui: &mut Ui,
    t: &Theme,
    data: &ForumBoardData<'_>,
    state: &mut ForumState,
    actions: &mut Vec<Action>,
) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;

        let new_post = primary_button(ui, t, "+ New Post", data.can_post);
        if data.can_post {
            if new_post.clicked() {
                actions.push(Action::OpenCreateThread {
                    parent_id: data.forum_id,
                    is_forum: true,
                });
            }
        } else {
            new_post.on_hover_text("You don't have permission to create posts here.");
        }

        // Search-posts field filling remaining width.
        let search_w = ui.available_width().max(140.0);
        let (field, _) = ui.allocate_exact_size(Vec2::new(search_w, 34.0), Sense::hover());
        ui.painter().rect(
            field,
            CornerRadius::same(6),
            t.elevated,
            Stroke::new(1.0, t.divider),
            egui::StrokeKind::Inside,
        );
        paint_search(
            ui,
            Pos2::new(field.left() + 14.0, field.center().y),
            14.0,
            t.muted,
        );
        let has_query = !state.search.is_empty();
        let right_pad = if has_query { 28.0 } else { 10.0 };
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_min_max(
                    Pos2::new(field.left() + 28.0, field.top()),
                    Pos2::new(field.right() - right_pad, field.bottom()),
                ))
                .layout(Layout::left_to_right(Align::Center)),
        );
        child.add(
            egui::TextEdit::singleline(&mut state.search)
                .hint_text("Search posts…")
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .font(prop(14.0))
                .desired_width(f32::INFINITY),
        );
        if has_query {
            let clear_rect = Rect::from_center_size(
                Pos2::new(field.right() - 14.0, field.center().y),
                Vec2::splat(20.0),
            );
            let clear_resp = ui
                .interact(
                    clear_rect,
                    ui.scope_id().with("forum_search_clear"),
                    Sense::click(),
                )
                .on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().text(
                clear_rect.center(),
                Align2::CENTER_CENTER,
                "×",
                bold(15.0),
                if clear_resp.hovered() {
                    t.strong
                } else {
                    t.muted
                },
            );
            if clear_resp.on_hover_text("Clear search").clicked() {
                state.search.clear();
            }
        }
    });
}

fn tag_filter_bar(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    available_tags: &[ForumTag],
    state: &mut ForumState,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        for tag in available_tags {
            let selected = state.selected_tags.contains(&tag.id);
            let label = tag_display_text(tag);
            let resp = tag_pill(ui, t, media, &label, selected, true);
            if resp.clicked() && !state.selected_tags.remove(&tag.id) {
                state.selected_tags.insert(tag.id);
            }
        }
        if state.has_filters() {
            let clear = tag_pill(ui, t, media, "Clear filters", false, true);
            if clear.clicked() {
                state.clear_filters();
            }
        }
    });
}

fn post_card(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    card: &PostCard,
    clock_24h: bool,
    actions: &mut Vec<Action>,
) {
    let mut prepared = Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 12))
        .begin(ui);

    {
        let ui = &mut prepared.content_ui;
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

        // Top row: unread dot + title (+ locked pill) on left; mention badge + timestamp on right.
        let stamp_ms = card
            .preview
            .as_ref()
            .map_or(card.activity_ms, |p| p.timestamp_ms);
        let stamp = fmt_stamp(stamp_ms, clock_24h);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if !stamp.is_empty() {
                ui.label(RichText::new(&stamp).font(prop(12.0)).color(t.muted));
            }
            if card.mentions > 0 {
                paint_inline_mention_badge(ui, card.mentions, t);
            }

            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                if card.unread {
                    let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 4.0, t.accent);
                }
                let title_color = if card.unread { t.strong } else { t.text };
                emoji_text::label_truncated(ui, media, &card.title, bold(16.0), title_color);
                if card.locked {
                    let g = ui
                        .painter()
                        .layout_no_wrap("Locked".into(), bold(10.5), t.muted);
                    let (r, _) =
                        ui.allocate_exact_size(Vec2::new(g.size().x + 12.0, 18.0), Sense::hover());
                    ui.painter().rect(
                        r,
                        CornerRadius::same(4),
                        t.hover,
                        Stroke::new(1.0, t.divider),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().galley(r.center() - g.size() / 2.0, g, t.muted);
                }
            });
        });

        // Recent message preview snippet (when cached) or subtle hint.
        if let Some(prev) = &card.preview {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let author_col = prev.author_color.unwrap_or(t.strong);
                emoji_text::label(
                    ui,
                    media,
                    &format!("{}:", prev.author_name),
                    bold(13.0),
                    author_col,
                );
                ui.add(
                    egui::Label::new(RichText::new(&prev.snippet).font(prop(13.5)).color(t.muted))
                        .truncate(),
                );
            });
        } else {
            ui.label(
                RichText::new("Open post to view messages")
                    .font(prop(13.0))
                    .color(t.muted.gamma_multiply(0.8)),
            );
        }

        // Applied tag pills.
        if !card.tags.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(5.0, 5.0);
                for tag_text in &card.tags {
                    tag_pill(ui, t, media, tag_text, false, false);
                }
            });
        }
    }

    let resp = prepared
        .allocate_space(ui)
        .interact(Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if resp.hovered() {
        prepared.frame.fill = t.selected;
        prepared.frame.stroke = Stroke::new(1.0, t.accent.gamma_multiply(0.55));
    }
    prepared.paint(ui);

    if resp.clicked() {
        actions.push(Action::SelectChannel(card.id));
    }
}

fn empty_forum_card(
    ui: &mut Ui,
    t: &Theme,
    forum_id: Snowflake,
    can_post: bool,
    actions: &mut Vec<Action>,
) {
    Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(28))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("No posts yet")
                        .font(bold(16.0))
                        .color(t.strong),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Start the first conversation in this forum.")
                        .font(prop(13.5))
                        .color(t.muted),
                );
                if can_post {
                    ui.add_space(12.0);
                    if primary_button(ui, t, "+ New Post", true).clicked() {
                        actions.push(Action::OpenCreateThread {
                            parent_id: forum_id,
                            is_forum: true,
                        });
                    }
                }
            });
        });
}

fn empty_filtered_card(ui: &mut Ui, t: &Theme, state: &mut ForumState) {
    Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(24))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("No posts match your filters")
                        .font(bold(15.5))
                        .color(t.strong),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Try adjusting your search query or clearing selected tags.")
                        .font(prop(13.0))
                        .color(t.muted),
                );
                ui.add_space(10.0);
                if secondary_button(ui, t, "Clear filters", true).clicked() {
                    state.clear_filters();
                }
            });
        });
}

// ---- create thread / new forum post modal -------------------------------------------

pub fn create_modal(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    let Some(parent_id) = app.view.create_thread.as_ref().map(|m| m.parent_id) else {
        return;
    };
    let (parent_name, available_tags) = app
        .store
        .channel(parent_id)
        .map(|c| (c.label(), c.available_tags.clone()))
        .unwrap_or_else(|| ("channel".into(), Vec::new()));

    let should_close = {
        let Some(modal) = app.view.create_thread.as_mut() else {
            return;
        };
        render_create_modal(
            ctx,
            app.theme,
            &app.media,
            &parent_name,
            &available_tags,
            modal,
            actions,
        )
    };
    if should_close {
        app.view.create_thread = None;
    }
}

/// Render the Create Thread / New Forum Post modal. Returns `true` when it should close.
fn render_create_modal(
    ctx: &egui::Context,
    t: &Theme,
    media: &RefCell<Media>,
    parent_name: &str,
    available_tags: &[ForumTag],
    modal: &mut CreateThreadModal,
    actions: &mut Vec<Action>,
) -> bool {
    let modal_w = (ctx.content_rect().width() - 40.0).clamp(320.0, 480.0);
    let mut close = false;
    let mut submit = false;

    let resp = egui::Modal::new(Id::unique("create-thread-modal"))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(modal_w);
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

            // Title & channel subtitle.
            let heading = if modal.is_forum {
                "New Forum Post"
            } else {
                "Create Public Thread"
            };
            ui.label(RichText::new(heading).font(bold(19.0)).color(t.strong));
            ui.label(
                RichText::new(format!("in #{parent_name}"))
                    .font(prop(13.0))
                    .color(t.muted),
            );
            ui.add_space(4.0);

            // Thread / Post title input.
            let title_chars = modal.title.trim().chars().count();
            field_header(
                ui,
                t,
                if modal.is_forum {
                    "POST TITLE"
                } else {
                    "THREAD NAME"
                },
                &format!("{title_chars}/{MAX_THREAD_NAME_CHARS}"),
                title_chars > MAX_THREAD_NAME_CHARS,
            );
            let title_resp = Frame::new()
                .fill(t.chat.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.add_enabled(
                        !modal.busy,
                        egui::TextEdit::singleline(&mut modal.title)
                            .hint_text(if modal.is_forum {
                                "Enter a post title…"
                            } else {
                                "Enter a thread name…"
                            })
                            .char_limit(MAX_THREAD_NAME_CHARS)
                            .frame(Frame::NONE)
                            .font(prop(14.5))
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;
            if modal.focus_title {
                title_resp.request_focus();
                modal.focus_title = false;
            }

            // Selectable tags (forum mode only).
            if modal.is_forum && !available_tags.is_empty() {
                ui.add_space(2.0);
                field_header(
                    ui,
                    t,
                    "TAGS",
                    &format!("{}/{MAX_FORUM_TAGS}", modal.selected_tags.len()),
                    false,
                );
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for tag in available_tags {
                        let selected = modal.selected_tags.contains(&tag.id);
                        let at_limit = !selected && modal.selected_tags.len() >= MAX_FORUM_TAGS;
                        let interactive = !modal.busy && !at_limit;
                        let label = tag_display_text(tag);
                        let mut pill_resp = tag_pill(ui, t, media, &label, selected, interactive);
                        if tag.moderated {
                            pill_resp = pill_resp.on_hover_text("Moderator-only tag");
                        } else if at_limit {
                            pill_resp = pill_resp
                                .on_hover_text(format!("Up to {MAX_FORUM_TAGS} tags per post"));
                        }
                        if interactive && pill_resp.clicked() {
                            if let Some(idx) =
                                modal.selected_tags.iter().position(|id| *id == tag.id)
                            {
                                modal.selected_tags.remove(idx);
                            } else if modal.selected_tags.len() < MAX_FORUM_TAGS {
                                modal.selected_tags.push(tag.id);
                            }
                        }
                    }
                });
            }

            // Starter message input (required for forum posts, optional for text threads).
            ui.add_space(2.0);
            let msg_chars = modal.message.trim().chars().count();
            field_header(
                ui,
                t,
                if modal.is_forum {
                    "MESSAGE"
                } else {
                    "STARTER MESSAGE (OPTIONAL)"
                },
                &format!("{msg_chars}/{MAX_STARTER_MSG_CHARS}"),
                msg_chars > MAX_STARTER_MSG_CHARS,
            );
            Frame::new()
                .fill(t.chat.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("create_thread_msg_scroll")
                        .max_height(150.0)
                        .show(ui, |ui| {
                            ui.add_enabled(
                                !modal.busy,
                                egui::TextEdit::multiline(&mut modal.message)
                                    .hint_text(if modal.is_forum {
                                        "Write the first message of your post…"
                                    } else {
                                        "Optional first message to send in the thread…"
                                    })
                                    .char_limit(MAX_STARTER_MSG_CHARS)
                                    .desired_rows(4)
                                    .frame(Frame::NONE)
                                    .font(prop(14.0))
                                    .desired_width(f32::INFINITY),
                            );
                        });
                });

            if let Some(err) = &modal.error {
                ui.label(RichText::new(err).font(prop(13.0)).color(t.danger));
            }

            ui.add_space(6.0);
            let valid = can_submit_create_thread(modal);
            if valid && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let submit_label = if modal.busy {
                    "Creating…"
                } else if modal.is_forum {
                    "Create Post"
                } else {
                    "Create Thread"
                };
                if primary_button(ui, t, submit_label, valid).clicked() {
                    submit = true;
                }
                if modal.busy {
                    ui.add(egui::Spinner::new().size(16.0).color(t.muted));
                }
                if secondary_button(ui, t, "Cancel", !modal.busy).clicked() {
                    close = true;
                }
            });
        });

    if submit && can_submit_create_thread(modal) {
        let name = modal.title.trim().to_string();
        let trimmed_msg = modal.message.trim();
        let message = (!trimmed_msg.is_empty()).then(|| trimmed_msg.to_string());
        let applied_tags = if modal.is_forum {
            modal.selected_tags.clone()
        } else {
            Vec::new()
        };
        modal.busy = true;
        modal.error = None;
        actions.push(Action::CreateThread {
            parent_id: modal.parent_id,
            is_forum: modal.is_forum,
            name,
            message,
            applied_tags,
        });
    }

    !modal.busy && (close || resp.should_close())
}

// ---- shared UI primitives -----------------------------------------------------------

fn field_header(ui: &mut Ui, t: &Theme, title: &str, counter: &str, warn: bool) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.label(RichText::new(counter).font(prop(11.5)).color(if warn {
            t.danger
        } else {
            t.muted
        }));
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.label(RichText::new(title).font(bold(11.5)).color(t.muted));
        });
    });
}

fn tag_pill(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    label: &str,
    selected: bool,
    clickable: bool,
) -> egui::Response {
    let fg = if selected { t.strong } else { t.text };
    let galley = emoji_text::layout(ui.painter(), label, prop(12.5), fg, f32::INFINITY, 0);
    let size = Vec2::new(galley.size().x + 18.0, 24.0);
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let hovered = clickable && resp.hovered();
    let (fill, stroke) = if selected {
        (t.accent.gamma_multiply(0.24), Stroke::new(1.2, t.accent))
    } else if hovered {
        (t.selected, Stroke::new(1.0, t.accent.gamma_multiply(0.5)))
    } else {
        (t.hover, Stroke::new(1.0, t.divider))
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(12),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let text_pos = Pos2::new(rect.left() + 9.0, rect.center().y - galley.size().y / 2.0);
    galley.paint(ui.painter(), text_pos, media);
    if clickable {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn paint_inline_mention_badge(ui: &mut Ui, count: u32, t: &Theme) {
    let text = badge_text(count);
    let g = ui
        .painter()
        .layout_no_wrap(text, bold(11.0), Color32::WHITE);
    let w = (g.size().x + 10.0).max(18.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), t.danger);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, Color32::WHITE);
}

fn primary_button(ui: &mut Ui, t: &Theme, label: &str, enabled: bool) -> egui::Response {
    let fg = if enabled {
        t.on_accent
    } else {
        t.on_accent.gamma_multiply(0.5)
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), bold(13.5), fg);
    let size = Vec2::new((g.size().x + 24.0).max(96.0), 34.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let fill = if !enabled {
        t.accent.gamma_multiply(0.4)
    } else if resp.hovered() {
        t.accent.gamma_multiply(0.88)
    } else {
        t.accent
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn secondary_button(ui: &mut Ui, t: &Theme, label: &str, enabled: bool) -> egui::Response {
    let fg = if enabled {
        t.strong
    } else {
        t.muted.gamma_multiply(0.5)
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), prop(13.5), fg);
    let size = Vec2::new((g.size().x + 20.0).max(80.0), 34.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let fill = if enabled && resp.hovered() {
        t.selected
    } else {
        t.hover
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{
        Attachment, ChannelKind, Embed, GatewayEvent, Guild, Ready, ThreadMetadata, User,
    };

    fn sample_tags() -> Vec<ForumTag> {
        vec![
            ForumTag {
                id: Snowflake(1),
                name: "Question".into(),
                emoji_name: Some("\u{2753}".into()),
                ..Default::default()
            },
            ForumTag {
                id: Snowflake(2),
                name: "egui".into(),
                emoji_name: Some("\u{1F3A8}".into()),
                ..Default::default()
            },
            ForumTag {
                id: Snowflake(3),
                name: "Solved".into(),
                emoji_name: Some("\u{2705}".into()),
                ..Default::default()
            },
        ]
    }

    fn sample_store() -> (Store, Snowflake, Vec<ForumTag>) {
        let tags = sample_tags();
        let forum_id = Snowflake(204);
        let me = User {
            id: Snowflake(10),
            username: "you".into(),
            global_name: Some("You".into()),
            ..Default::default()
        };
        let sol = User {
            id: Snowflake(11),
            username: "sol_dev".into(),
            global_name: Some("Sol".into()),
            ..Default::default()
        };
        let forum = Channel {
            id: forum_id,
            kind: ChannelKind::Forum,
            guild_id: Some(Snowflake(20)),
            name: Some("help-forum".into()),
            topic: Some("Ask Rust & egui questions".into()),
            available_tags: tags.clone(),
            ..Default::default()
        };
        let post_a = Channel {
            id: Snowflake(207),
            kind: ChannelKind::PublicThread,
            guild_id: Some(Snowflake(20)),
            parent_id: Some(forum_id),
            name: Some("Keeping memory low on Windows".into()),
            applied_tags: vec![Snowflake(1), Snowflake(2)],
            last_message_id: Some(Snowflake(900)),
            thread_metadata: Some(ThreadMetadata::default()),
            ..Default::default()
        };
        let post_b = Channel {
            id: Snowflake(208),
            kind: ChannelKind::PublicThread,
            guild_id: Some(Snowflake(20)),
            parent_id: Some(forum_id),
            name: Some("Clean tokio shutdown".into()),
            applied_tags: vec![Snowflake(3)],
            last_message_id: Some(Snowflake(800)),
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: true,
            }),
            ..Default::default()
        };
        let mut store = Store::new();
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me,
            guilds: vec![Guild {
                id: Snowflake(20),
                name: "Rust Lounge".into(),
                channels: vec![forum],
                threads: vec![post_a, post_b],
                ..Default::default()
            }],
            session_id: "test".into(),
            ..Default::default()
        })));
        store.insert_history(
            Snowflake(207),
            vec![Message {
                id: Snowflake(900),
                channel_id: Snowflake(207),
                guild_id: Some(Snowflake(20)),
                author: sol,
                content: "Try trimming the working set once after startup.".into(),
                timestamp: "2026-11-23T08:00:00Z".into(),
                ..Default::default()
            }],
        );
        (store, forum_id, tags)
    }

    #[test]
    fn tag_filtering_requires_all_selected_tags() {
        let (store, forum_id, tags) = sample_store();
        let threads = store.channel_threads(forum_id);
        assert_eq!(threads.len(), 2);

        let mut selected = HashSet::new();
        selected.insert(Snowflake(1));
        let out = filter_posts(&threads, &tags, &store, "", &selected);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, Snowflake(207));

        // Selecting both tag 1 and tag 2 still matches post 207.
        selected.insert(Snowflake(2));
        let out = filter_posts(&threads, &tags, &store, "", &selected);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, Snowflake(207));

        // Adding tag 3 (only on post 208) yields 0 matches under AND semantics.
        selected.insert(Snowflake(3));
        let out = filter_posts(&threads, &tags, &store, "", &selected);
        assert!(out.is_empty());
    }

    #[test]
    fn search_filters_by_title_tag_name_and_cached_message() {
        let (store, forum_id, tags) = sample_store();
        let threads = store.channel_threads(forum_id);
        let empty_tags = HashSet::new();

        // Title match (case-insensitive).
        let by_title = filter_posts(&threads, &tags, &store, "TOKIO", &empty_tags);
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].id, Snowflake(208));

        // Applied tag name match.
        let by_tag = filter_posts(&threads, &tags, &store, "solved", &empty_tags);
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].id, Snowflake(208));

        // Cached message body match.
        let by_msg = filter_posts(&threads, &tags, &store, "working set", &empty_tags);
        assert_eq!(by_msg.len(), 1);
        assert_eq!(by_msg[0].id, Snowflake(207));

        // Cached message author match.
        let by_author = filter_posts(&threads, &tags, &store, "sol_dev", &empty_tags);
        assert_eq!(by_author.len(), 1);
        assert_eq!(by_author[0].id, Snowflake(207));

        // Non-matching query returns empty.
        let none = filter_posts(&threads, &tags, &store, "nonexistent", &empty_tags);
        assert!(none.is_empty());
    }

    #[test]
    fn forum_state_resets_when_switching_forums() {
        let mut st = ForumState::default();
        st.sync_forum(Snowflake(204));
        st.search = "egui".into();
        st.selected_tags.insert(Snowflake(1));
        assert!(st.has_filters());

        // Same forum keeps filters.
        st.sync_forum(Snowflake(204));
        assert_eq!(st.search, "egui");
        assert_eq!(st.selected_tags.len(), 1);

        // Switching to another forum clears filters.
        st.sync_forum(Snowflake(305));
        assert_eq!(st.forum_id, Some(Snowflake(305)));
        assert!(!st.has_filters());
    }

    #[test]
    fn create_thread_validation_rules() {
        // Forum post requires both title and starter message.
        let mut forum_modal = CreateThreadModal::new(Snowflake(204), true);
        assert!(!can_submit_create_thread(&forum_modal));
        forum_modal.title = "   ".into();
        forum_modal.message = "Hello".into();
        assert!(!can_submit_create_thread(&forum_modal));
        forum_modal.title = "Need help with egui".into();
        forum_modal.message = "   ".into();
        assert!(!can_submit_create_thread(&forum_modal));
        forum_modal.message = "How do I center a modal?".into();
        assert!(can_submit_create_thread(&forum_modal));

        // Busy blocks submit.
        forum_modal.busy = true;
        assert!(!can_submit_create_thread(&forum_modal));
        forum_modal.busy = false;

        // Length limits enforced.
        forum_modal.title = "a".repeat(MAX_THREAD_NAME_CHARS + 1);
        assert!(!can_submit_create_thread(&forum_modal));
        forum_modal.title = "a".repeat(MAX_THREAD_NAME_CHARS);
        forum_modal.message = "b".repeat(MAX_STARTER_MSG_CHARS + 1);
        assert!(!can_submit_create_thread(&forum_modal));

        // Regular text-channel thread allows empty starter message.
        let mut text_modal = CreateThreadModal::new(Snowflake(102), false);
        assert!(!can_submit_create_thread(&text_modal));
        text_modal.title = "v0.3-planning".into();
        assert!(can_submit_create_thread(&text_modal));
        text_modal.message = "First note".into();
        assert!(can_submit_create_thread(&text_modal));
    }

    #[test]
    fn single_line_preview_handles_text_attachments_and_embeds() {
        let mut m = Message {
            content: "hello\n   world\tthere".into(),
            ..Default::default()
        };
        assert_eq!(single_line_preview(&m), "hello world there");

        m.content = "x".repeat(PREVIEW_MAX_CHARS + 20);
        let p = single_line_preview(&m);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), PREVIEW_MAX_CHARS + 1);

        m.content.clear();
        m.attachments.push(Attachment {
            filename: "trace.log".into(),
            ..Default::default()
        });
        assert_eq!(single_line_preview(&m), "[Attachment: trace.log]");
        m.attachments.push(Attachment {
            filename: "shot.png".into(),
            ..Default::default()
        });
        assert_eq!(single_line_preview(&m), "[2 attachments]");

        m.attachments.clear();
        m.embeds.push(Embed {
            title: Some("Build passed".into()),
            ..Default::default()
        });
        assert_eq!(single_line_preview(&m), "[Embed: Build passed]");
    }

    #[test]
    fn headless_forum_board_and_modals_render_cleanly() {
        let ctx = egui::Context::default();
        let t = theme::by_name("Graphite");
        let media = RefCell::new(Media::new(&ctx, true));
        let tags = sample_tags();
        let cards = vec![
            PostCard {
                id: Snowflake(207),
                title: "Keeping memory low on Windows".into(),
                locked: false,
                unread: true,
                mentions: 2,
                tags: vec!["\u{2753} Question".into(), "\u{1F3A8} egui".into()],
                preview: Some(PostPreview {
                    author_name: "Sol".into(),
                    author_color: Some(Color32::from_rgb(0xE7, 0x4C, 0x3C)),
                    snippet: "Try trimming the working set once after startup.".into(),
                    timestamp_ms: 1_763_884_800_000,
                }),
                activity_ms: 1_763_884_800_000,
            },
            PostCard {
                id: Snowflake(208),
                title: "Clean tokio shutdown".into(),
                locked: true,
                unread: false,
                mentions: 0,
                tags: vec!["\u{2705} Solved".into()],
                preview: None,
                activity_ms: 1_763_884_200_000,
            },
        ];

        // 1. Populated forum board renders cleanly.
        let mut state = ForumState::default();
        state.sync_forum(Snowflake(204));
        let mut actions = Vec::new();
        let data = ForumBoardData {
            forum_id: Snowflake(204),
            name: "help-forum",
            topic: Some("Ask Rust & egui questions"),
            available_tags: &tags,
            can_post: true,
            total_posts: 2,
            cards: &cards,
            clock_24h: true,
            members_shown: false,
        };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            render_forum_board(ui, t, &media, &data, &mut state, &mut actions);
        });
        out.textures_delta.clear();
        assert!(actions.is_empty());

        // 2. Empty filtered state renders cleanly.
        state.search = "unmatched".into();
        let filtered_empty = ForumBoardData { cards: &[], ..data };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            render_forum_board(ui, t, &media, &filtered_empty, &mut state, &mut actions);
        });
        out.textures_delta.clear();

        // 3. Empty forum state renders cleanly.
        state.clear_filters();
        let zero_posts = ForumBoardData {
            total_posts: 0,
            cards: &[],
            ..data
        };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            render_forum_board(ui, t, &media, &zero_posts, &mut state, &mut actions);
        });
        out.textures_delta.clear();

        // 4. Create modal in both Forum and Text-channel modes renders cleanly.
        let mut forum_modal = CreateThreadModal::new(Snowflake(204), true);
        forum_modal.title = "Demo post".into();
        forum_modal.message = "First message".into();
        forum_modal.selected_tags.push(Snowflake(1));
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = render_create_modal(
                ui.ctx(),
                t,
                &media,
                "help-forum",
                &tags,
                &mut forum_modal,
                &mut actions,
            );
        });
        out.textures_delta.clear();

        let mut text_modal = CreateThreadModal::new(Snowflake(102), false);
        text_modal.title = "v0.3-roadmap".into();
        text_modal.error = Some("Sample error".into());
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = render_create_modal(
                ui.ctx(),
                t,
                &media,
                "general",
                &[],
                &mut text_modal,
                &mut actions,
            );
        });
        out.textures_delta.clear();
    }
}

//! Guild rail, channel column (guild channels or DM list) and the user panel.

use super::{
    bold, emoji_text, icon_button, id_color, initials, paint_chevron, paint_plus, paint_search,
    paint_status_dot, prop, Action, Icon,
};
use crate::app::NixApp;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Key, Layout, Margin, Modal,
    Order, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2,
};
use nixcord_model::{Channel, ChannelKind, Guild, GuildFolder, Snowflake};
use nixcord_store::ConnectionState;
use std::borrow::Cow;
use std::collections::HashSet;

// ---- mention badge ------------------------------------------------------------------

fn badge_text(count: u32) -> String {
    if count > 99 {
        "99+".into()
    } else {
        count.to_string()
    }
}

/// Paint a Discord-style red mention pill (`count` > 0) with its right-center at
/// `right_center`. If `ring` is `Some`, paints a cutout ring behind it (used on the
/// guild rail icons). Returns the pill width (0 when `count == 0`).
fn paint_mention_badge(
    ui: &Ui,
    right_center: Pos2,
    count: u32,
    ring: Option<Color32>,
    t: &Theme,
) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let text = badge_text(count);
    let g = ui
        .painter()
        .layout_no_wrap(text, bold(11.0), Color32::WHITE);
    let w = (g.size().x + 8.0).max(16.0);
    let h = 16.0;
    let rect = Rect::from_min_size(
        Pos2::new(right_center.x - w, right_center.y - h / 2.0),
        Vec2::new(w, h),
    );
    if let Some(bg) = ring {
        ui.painter()
            .rect_filled(rect.expand(2.5), CornerRadius::same(10), bg);
    }
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), t.danger);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, Color32::WHITE);
    w
}

/// Width reserved for a right-aligned mention badge in a channel/DM row.
fn badge_reserve(count: u32) -> f32 {
    if count == 0 {
        0.0
    } else if count > 99 {
        36.0
    } else if count >= 10 {
        28.0
    } else {
        22.0
    }
}

// ---- right-click context menu & server rail layout ---------------------------------

/// Item currently being dragged on the server rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailDrag {
    Guild(Snowflake),
    Folder(u64),
}

/// Drop zone hovered while dragging a server or folder on the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailDropTarget {
    BeforeTopGuild(Snowflake),
    AfterTopGuild(Snowflake),
    GroupWithGuild(Snowflake),
    BeforeFolder(u64),
    AfterFolder(u64),
    IntoFolder(u64),
    InsideFolderBefore {
        folder_id: u64,
        target_guild: Snowflake,
    },
    InsideFolderAfter {
        folder_id: u64,
        target_guild: Snowflake,
    },
}

/// Mutation to the server rail's folder/order layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailOp {
    ToggleFolder(u64),
    MoveGuild {
        guild_id: Snowflake,
        delta: i32,
    },
    CreateFolder(Snowflake),
    MoveGuildToFolder {
        guild_id: Snowflake,
        folder_id: u64,
    },
    RemoveFromFolder(Snowflake),
    MoveFolder {
        folder_id: u64,
        delta: i32,
    },
    EditFolder(u64),
    UngroupFolder(u64),
    MarkFolderRead(u64),
    Drop {
        source: RailDrag,
        target: RailDropTarget,
    },
}

/// Modal state for editing a server folder's name and color.
#[derive(Debug, Clone)]
pub struct FolderModal {
    pub folder_id: u64,
    pub name: String,
    pub color: Option<u32>,
    pub focus_name: bool,
}

pub const FOLDER_SWATCHES: &[(Option<u32>, &str)] = &[
    (None, "Theme Accent"),
    (Some(0x5865F2), "Blurple"),
    (Some(0x3BA55C), "Emerald"),
    (Some(0xFAA61A), "Amber"),
    (Some(0xED4245), "Coral"),
    (Some(0xEB459E), "Fuchsia"),
    (Some(0x1ABC9C), "Teal"),
    (Some(0x9B59B6), "Amethyst"),
];

fn rgb_u32(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[derive(Debug, Clone)]
enum MenuEffect {
    Action(Action),
    Rail(RailOp),
}

struct ContextMenuItem {
    label: Cow<'static, str>,
    enabled: bool,
    action: Option<Action>,
    rail_op: Option<RailOp>,
    divider_after: bool,
}

impl ContextMenuItem {
    fn action(
        label: impl Into<Cow<'static, str>>,
        enabled: bool,
        action: Option<Action>,
        divider_after: bool,
    ) -> Self {
        Self {
            label: label.into(),
            enabled,
            action,
            rail_op: None,
            divider_after,
        }
    }

    fn rail(
        label: impl Into<Cow<'static, str>>,
        enabled: bool,
        op: Option<RailOp>,
        divider_after: bool,
    ) -> Self {
        Self {
            label: label.into(),
            enabled,
            action: None,
            rail_op: op,
            divider_after,
        }
    }
}

fn show_context_menu(
    resp: &egui::Response,
    t: &Theme,
    items: &[ContextMenuItem],
    actions: &mut Vec<Action>,
) {
    let mut rail_ops = Vec::new();
    show_context_menu_full(resp, t, items, actions, &mut rail_ops);
}

fn show_context_menu_full(
    resp: &egui::Response,
    t: &Theme,
    items: &[ContextMenuItem],
    actions: &mut Vec<Action>,
    rail_ops: &mut Vec<RailOp>,
) {
    let mut picked: Option<MenuEffect> = None;
    egui::Popup::context_menu(resp).width(212.0).show(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for item in items {
            let (rect, r) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 28.0),
                if item.enabled {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            );
            if item.enabled && r.hovered() {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(4), t.hover);
            }
            let fg = if !item.enabled {
                t.muted.gamma_multiply(0.45)
            } else if r.hovered() {
                t.strong
            } else {
                t.text
            };
            ui.painter().text(
                Pos2::new(rect.left() + 10.0, rect.center().y),
                Align2::LEFT_CENTER,
                item.label.as_ref(),
                prop(13.5),
                fg,
            );
            if item.enabled {
                let r = r.on_hover_cursor(CursorIcon::PointingHand);
                if r.clicked() {
                    if let Some(a) = &item.action {
                        picked = Some(MenuEffect::Action(a.clone()));
                    } else if let Some(op) = &item.rail_op {
                        picked = Some(MenuEffect::Rail(op.clone()));
                    }
                }
            }
            if item.divider_after {
                let (sep, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                ui.painter().line_segment(
                    [
                        Pos2::new(sep.left() + 6.0, sep.center().y),
                        Pos2::new(sep.right() - 6.0, sep.center().y),
                    ],
                    Stroke::new(1.0, t.divider),
                );
            }
        }
    });
    if let Some(eff) = picked {
        match eff {
            MenuEffect::Action(a) => actions.push(a),
            MenuEffect::Rail(op) => rail_ops.push(op),
        }
        egui::Popup::close_all(&resp.ctx);
    }
}

fn guild_rail_context_items(
    guild_id: Snowflake,
    can_mark_read: bool,
    layout: &[GuildFolder],
) -> Vec<ContextMenuItem> {
    let (can_up, can_down, in_folder) = can_move_guild(layout, guild_id);
    let other_folders: Vec<(u64, String)> = layout
        .iter()
        .filter(|f| f.is_folder())
        .filter_map(|f| {
            let fid = f.id?;
            (Some(fid) != in_folder).then(|| (fid, f.display_name().to_string()))
        })
        .take(4)
        .collect();

    let mut items = vec![
        ContextMenuItem::action(
            "Mark Server as Read",
            can_mark_read,
            can_mark_read.then_some(Action::MarkGuildRead(guild_id)),
            true,
        ),
        ContextMenuItem::rail(
            "Move Server Up",
            can_up,
            can_up.then_some(RailOp::MoveGuild {
                guild_id,
                delta: -1,
            }),
            false,
        ),
        ContextMenuItem::rail(
            "Move Server Down",
            can_down,
            can_down.then_some(RailOp::MoveGuild { guild_id, delta: 1 }),
            true,
        ),
        ContextMenuItem::rail(
            "Create Folder with Server…",
            true,
            Some(RailOp::CreateFolder(guild_id)),
            other_folders.is_empty() && in_folder.is_none(),
        ),
    ];
    for (idx, (fid, fname)) in other_folders.iter().enumerate() {
        let last = idx + 1 == other_folders.len() && in_folder.is_none();
        items.push(ContextMenuItem::rail(
            format!("Move to {fname}"),
            true,
            Some(RailOp::MoveGuildToFolder {
                guild_id,
                folder_id: *fid,
            }),
            last,
        ));
    }
    if in_folder.is_some() {
        items.push(ContextMenuItem::rail(
            "Remove from Folder",
            true,
            Some(RailOp::RemoveFromFolder(guild_id)),
            true,
        ));
    }
    items.push(ContextMenuItem::action(
        "Copy Server ID",
        true,
        Some(Action::Copy(guild_id.to_string())),
        false,
    ));
    items
}

fn folder_context_items(
    folder_id: u64,
    can_mark_read: bool,
    layout: &[GuildFolder],
) -> Vec<ContextMenuItem> {
    let (can_up, can_down) = can_move_folder(layout, folder_id);
    vec![
        ContextMenuItem::rail(
            "Mark Folder as Read",
            can_mark_read,
            can_mark_read.then_some(RailOp::MarkFolderRead(folder_id)),
            true,
        ),
        ContextMenuItem::rail(
            "Folder Settings…",
            true,
            Some(RailOp::EditFolder(folder_id)),
            true,
        ),
        ContextMenuItem::rail(
            "Move Folder Up",
            can_up,
            can_up.then_some(RailOp::MoveFolder {
                folder_id,
                delta: -1,
            }),
            false,
        ),
        ContextMenuItem::rail(
            "Move Folder Down",
            can_down,
            can_down.then_some(RailOp::MoveFolder {
                folder_id,
                delta: 1,
            }),
            true,
        ),
        ContextMenuItem::rail(
            "Ungroup Folder",
            true,
            Some(RailOp::UngroupFolder(folder_id)),
            false,
        ),
    ]
}

fn channel_context_items(channel_id: Snowflake, can_mark_read: bool) -> [ContextMenuItem; 2] {
    [
        ContextMenuItem::action(
            "Mark as Read",
            can_mark_read,
            can_mark_read.then_some(Action::MarkChannelRead(channel_id)),
            true,
        ),
        ContextMenuItem::action(
            "Copy ID",
            true,
            Some(Action::Copy(channel_id.to_string())),
            false,
        ),
    ]
}

fn thread_context_items(
    thread_id: Snowflake,
    can_mark_read: bool,
    is_archived: bool,
    is_locked: bool,
) -> [ContextMenuItem; 4] {
    [
        ContextMenuItem::action(
            "Mark as Read",
            can_mark_read,
            can_mark_read.then_some(Action::MarkChannelRead(thread_id)),
            true,
        ),
        ContextMenuItem::action(
            if is_archived {
                "Unarchive Thread"
            } else {
                "Archive Thread"
            },
            true,
            Some(Action::UpdateThread {
                thread_id,
                archived: Some(!is_archived),
                locked: None,
            }),
            false,
        ),
        ContextMenuItem::action(
            if is_locked {
                "Unlock Thread"
            } else {
                "Lock Thread"
            },
            true,
            Some(Action::UpdateThread {
                thread_id,
                archived: None,
                locked: Some(!is_locked),
            }),
            true,
        ),
        ContextMenuItem::action(
            "Copy ID",
            true,
            Some(Action::Copy(thread_id.to_string())),
            false,
        ),
    ]
}

// ---- pure server rail layout engine -------------------------------------------------

fn next_folder_id(layout: &[GuildFolder]) -> u64 {
    let max_id = layout.iter().filter_map(|f| f.id).max().unwrap_or(100);
    max_id.saturating_add(1)
}

/// Synchronize persisted `layout` with the available guilds in the store (seeding from
/// `remote_folders` when `layout` is empty). Removes left guilds, deduplicates, assigns
/// missing folder IDs, and appends newly joined guilds at the bottom.
pub fn reconcile_guild_layout(
    layout: &mut Vec<GuildFolder>,
    remote_folders: &[GuildFolder],
    available_guilds: &[Snowflake],
) {
    if layout.is_empty() && !remote_folders.is_empty() {
        *layout = remote_folders.to_vec();
    }
    let avail: HashSet<Snowflake> = available_guilds.iter().copied().collect();
    let mut seen: HashSet<Snowflake> = HashSet::with_capacity(available_guilds.len());
    let mut max_fid = layout.iter().filter_map(|f| f.id).max().unwrap_or(100);

    for entry in layout.iter_mut() {
        let was_folder = entry.is_folder();
        entry
            .guild_ids
            .retain(|gid| avail.contains(gid) && seen.insert(*gid));
        if was_folder && entry.id.is_none() && !entry.guild_ids.is_empty() {
            max_fid = max_fid.saturating_add(1);
            entry.id = Some(max_fid);
        }
    }
    layout.retain(|entry| !entry.guild_ids.is_empty());

    for &gid in available_guilds {
        if seen.insert(gid) {
            layout.push(GuildFolder {
                id: None,
                guild_ids: vec![gid],
                name: None,
                color: None,
            });
        }
    }
}

/// Returns `(can_move_up, can_move_down, enclosing_folder_id)` for `guild_id`.
pub fn can_move_guild(layout: &[GuildFolder], guild_id: Snowflake) -> (bool, bool, Option<u64>) {
    for (top_idx, entry) in layout.iter().enumerate() {
        if let Some(pos) = entry.guild_ids.iter().position(|&g| g == guild_id) {
            if entry.is_folder() {
                if entry.guild_ids.len() > 1 {
                    return (pos > 0, pos + 1 < entry.guild_ids.len(), entry.id);
                }
                return (top_idx > 0, top_idx + 1 < layout.len(), entry.id);
            }
            return (top_idx > 0, top_idx + 1 < layout.len(), None);
        }
    }
    (false, false, None)
}

/// Returns `(can_move_up, can_move_down)` for `folder_id`.
pub fn can_move_folder(layout: &[GuildFolder], folder_id: u64) -> (bool, bool) {
    if let Some(idx) = layout.iter().position(|f| f.id == Some(folder_id)) {
        (idx > 0, idx + 1 < layout.len())
    } else {
        (false, false)
    }
}

/// Remove `guild_id` from wherever it lives in `layout`, cleaning up any emptied entries.
/// If a folder without a custom name or color drops to 1 guild, it unwraps back to a
/// standalone server automatically.
fn detach_guild(layout: &mut Vec<GuildFolder>, guild_id: Snowflake) {
    for entry in layout.iter_mut() {
        entry.guild_ids.retain(|&g| g != guild_id);
        let unnamed = entry.name.as_deref().is_none_or(|s| s.trim().is_empty());
        if entry.guild_ids.len() == 1 && unnamed && entry.color.is_none() {
            entry.id = None;
            entry.name = None;
        }
    }
    layout.retain(|entry| !entry.guild_ids.is_empty());
}

/// Apply one [`RailOp`] to `layout` and `collapsed`. Returns `Some(folder_id)` if the
/// caller should open the Folder Settings modal for `folder_id`.
pub fn apply_rail_op(
    layout: &mut Vec<GuildFolder>,
    collapsed: &mut HashSet<u64>,
    op: RailOp,
) -> Option<u64> {
    match op {
        RailOp::ToggleFolder(fid) => {
            if !collapsed.remove(&fid) {
                collapsed.insert(fid);
            }
            None
        }
        RailOp::EditFolder(fid) => Some(fid),
        RailOp::MarkFolderRead(_) => None,
        RailOp::MoveGuild { guild_id, delta } => {
            for i in 0..layout.len() {
                if let Some(pos) = layout[i].guild_ids.iter().position(|&g| g == guild_id) {
                    if layout[i].is_folder() && layout[i].guild_ids.len() > 1 {
                        let new_pos = (pos as i32 + delta)
                            .clamp(0, layout[i].guild_ids.len() as i32 - 1)
                            as usize;
                        layout[i].guild_ids.swap(pos, new_pos);
                    } else {
                        let new_i = (i as i32 + delta).clamp(0, layout.len() as i32 - 1) as usize;
                        layout.swap(i, new_i);
                    }
                    break;
                }
            }
            None
        }
        RailOp::MoveFolder { folder_id, delta } => {
            if let Some(i) = layout.iter().position(|f| f.id == Some(folder_id)) {
                let new_i = (i as i32 + delta).clamp(0, layout.len() as i32 - 1) as usize;
                layout.swap(i, new_i);
            }
            None
        }
        RailOp::CreateFolder(guild_id) => {
            let new_id = next_folder_id(layout);
            for i in 0..layout.len() {
                if layout[i].guild_ids.contains(&guild_id) {
                    if !layout[i].is_folder() {
                        layout[i].id = Some(new_id);
                        layout[i].name = Some("Server Folder".into());
                    } else {
                        layout[i].guild_ids.retain(|&g| g != guild_id);
                        let insert_at = if layout[i].guild_ids.is_empty() {
                            layout.remove(i);
                            i
                        } else {
                            i + 1
                        };
                        layout.insert(
                            insert_at,
                            GuildFolder {
                                id: Some(new_id),
                                guild_ids: vec![guild_id],
                                name: Some("Server Folder".into()),
                                color: None,
                            },
                        );
                    }
                    collapsed.remove(&new_id);
                    return Some(new_id);
                }
            }
            None
        }
        RailOp::MoveGuildToFolder {
            guild_id,
            folder_id,
        } => {
            if layout.iter().any(|f| f.id == Some(folder_id)) {
                detach_guild(layout, guild_id);
                if let Some(folder) = layout.iter_mut().find(|f| f.id == Some(folder_id)) {
                    if !folder.guild_ids.contains(&guild_id) {
                        folder.guild_ids.push(guild_id);
                    }
                }
                collapsed.remove(&folder_id);
            }
            None
        }
        RailOp::RemoveFromFolder(guild_id) => {
            if let Some(i) = layout
                .iter()
                .position(|f| f.is_folder() && f.guild_ids.contains(&guild_id))
            {
                layout[i].guild_ids.retain(|&g| g != guild_id);
                let unnamed = layout[i]
                    .name
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty());
                if layout[i].guild_ids.len() == 1 && unnamed && layout[i].color.is_none() {
                    layout[i].id = None;
                    layout[i].name = None;
                }
                let insert_at = if layout[i].guild_ids.is_empty() {
                    layout.remove(i);
                    i
                } else {
                    i + 1
                };
                layout.insert(
                    insert_at,
                    GuildFolder {
                        id: None,
                        guild_ids: vec![guild_id],
                        name: None,
                        color: None,
                    },
                );
            }
            None
        }
        RailOp::UngroupFolder(folder_id) => {
            if let Some(i) = layout.iter().position(|f| f.id == Some(folder_id)) {
                let folder = layout.remove(i);
                collapsed.remove(&folder_id);
                for (offset, gid) in folder.guild_ids.into_iter().enumerate() {
                    layout.insert(
                        i + offset,
                        GuildFolder {
                            id: None,
                            guild_ids: vec![gid],
                            name: None,
                            color: None,
                        },
                    );
                }
            }
            None
        }
        RailOp::Drop { source, target } => {
            apply_rail_drop(layout, collapsed, source, target);
            None
        }
    }
}

fn apply_rail_drop(
    layout: &mut Vec<GuildFolder>,
    collapsed: &mut HashSet<u64>,
    source: RailDrag,
    target: RailDropTarget,
) {
    match (source, target) {
        (RailDrag::Guild(src_gid), RailDropTarget::BeforeTopGuild(dst_gid))
            if src_gid != dst_gid =>
        {
            detach_guild(layout, src_gid);
            if let Some(i) = layout.iter().position(|f| f.guild_ids.contains(&dst_gid)) {
                layout.insert(
                    i,
                    GuildFolder {
                        id: None,
                        guild_ids: vec![src_gid],
                        name: None,
                        color: None,
                    },
                );
            }
        }
        (RailDrag::Guild(src_gid), RailDropTarget::AfterTopGuild(dst_gid))
            if src_gid != dst_gid =>
        {
            detach_guild(layout, src_gid);
            if let Some(i) = layout.iter().position(|f| f.guild_ids.contains(&dst_gid)) {
                layout.insert(
                    i + 1,
                    GuildFolder {
                        id: None,
                        guild_ids: vec![src_gid],
                        name: None,
                        color: None,
                    },
                );
            }
        }
        (RailDrag::Guild(src_gid), RailDropTarget::GroupWithGuild(dst_gid))
            if src_gid != dst_gid =>
        {
            let new_fid = next_folder_id(layout);
            detach_guild(layout, src_gid);
            if let Some(entry) = layout.iter_mut().find(|f| f.guild_ids.contains(&dst_gid)) {
                let fid = *entry.id.get_or_insert(new_fid);
                if let Some(pos) = entry.guild_ids.iter().position(|&g| g == dst_gid) {
                    entry.guild_ids.insert(pos + 1, src_gid);
                } else {
                    entry.guild_ids.push(src_gid);
                }
                collapsed.remove(&fid);
            }
        }
        (RailDrag::Guild(src_gid), RailDropTarget::BeforeFolder(fid)) => {
            detach_guild(layout, src_gid);
            if let Some(i) = layout.iter().position(|f| f.id == Some(fid)) {
                layout.insert(
                    i,
                    GuildFolder {
                        id: None,
                        guild_ids: vec![src_gid],
                        name: None,
                        color: None,
                    },
                );
            }
        }
        (RailDrag::Guild(src_gid), RailDropTarget::AfterFolder(fid)) => {
            detach_guild(layout, src_gid);
            if let Some(i) = layout.iter().position(|f| f.id == Some(fid)) {
                layout.insert(
                    i + 1,
                    GuildFolder {
                        id: None,
                        guild_ids: vec![src_gid],
                        name: None,
                        color: None,
                    },
                );
            }
        }
        (RailDrag::Guild(src_gid), RailDropTarget::IntoFolder(fid)) => {
            if layout.iter().any(|f| f.id == Some(fid)) {
                detach_guild(layout, src_gid);
                if let Some(folder) = layout.iter_mut().find(|f| f.id == Some(fid)) {
                    if !folder.guild_ids.contains(&src_gid) {
                        folder.guild_ids.push(src_gid);
                    }
                }
                collapsed.remove(&fid);
            }
        }
        (
            RailDrag::Guild(src_gid),
            RailDropTarget::InsideFolderBefore {
                folder_id,
                target_guild,
            },
        ) if src_gid != target_guild => {
            detach_guild(layout, src_gid);
            if let Some(folder) = layout.iter_mut().find(|f| f.id == Some(folder_id)) {
                let pos = folder
                    .guild_ids
                    .iter()
                    .position(|&g| g == target_guild)
                    .unwrap_or(folder.guild_ids.len());
                folder.guild_ids.insert(pos, src_gid);
            }
        }
        (
            RailDrag::Guild(src_gid),
            RailDropTarget::InsideFolderAfter {
                folder_id,
                target_guild,
            },
        ) if src_gid != target_guild => {
            detach_guild(layout, src_gid);
            if let Some(folder) = layout.iter_mut().find(|f| f.id == Some(folder_id)) {
                let pos = folder
                    .guild_ids
                    .iter()
                    .position(|&g| g == target_guild)
                    .map_or(folder.guild_ids.len(), |p| p + 1);
                folder.guild_ids.insert(pos, src_gid);
            }
        }
        (RailDrag::Folder(src_fid), target) => {
            let Some(src_idx) = layout.iter().position(|f| f.id == Some(src_fid)) else {
                return;
            };
            let is_same = matches!(
                target,
                RailDropTarget::BeforeFolder(fid)
                    | RailDropTarget::AfterFolder(fid)
                    | RailDropTarget::IntoFolder(fid)
                    if fid == src_fid
            );
            if is_same {
                return;
            }
            let folder = layout.remove(src_idx);
            let insert_idx = match target {
                RailDropTarget::BeforeTopGuild(gid) => {
                    layout.iter().position(|f| f.guild_ids.contains(&gid))
                }
                RailDropTarget::AfterTopGuild(gid) | RailDropTarget::GroupWithGuild(gid) => layout
                    .iter()
                    .position(|f| f.guild_ids.contains(&gid))
                    .map(|i| i + 1),
                RailDropTarget::BeforeFolder(fid) => layout.iter().position(|f| f.id == Some(fid)),
                RailDropTarget::AfterFolder(fid)
                | RailDropTarget::IntoFolder(fid)
                | RailDropTarget::InsideFolderBefore { folder_id: fid, .. }
                | RailDropTarget::InsideFolderAfter { folder_id: fid, .. } => {
                    layout.iter().position(|f| f.id == Some(fid)).map(|i| i + 1)
                }
            }
            .unwrap_or(src_idx.min(layout.len()));
            layout.insert(insert_idx, folder);
        }
        _ => {}
    }
}

// ---- guild rail ---------------------------------------------------------------------

pub fn rail(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Reconcile only once READY has arrived: before that the guild list is empty and would
    // wipe the saved layout. Unavailable guilds (outages) keep their place; they're just
    // not drawn.
    if app.store.me().is_some() {
        let known_guilds: Vec<Snowflake> = app.store.guilds().into_iter().map(|g| g.id).collect();
        let remote_folders = app.store.guild_folders().to_vec();
        reconcile_guild_layout(&mut app.prefs.guild_layout, &remote_folders, &known_guilds);
    }

    egui::Panel::bottom("rail_user_card")
        .exact_size(USER_CARD_RESERVE)
        .show_separator_line(false)
        .frame(Frame::new())
        .show(ui, |_| {});

    egui::Panel::bottom("rail_add")
        .exact_size(64.0)
        .show_separator_line(false)
        .frame(Frame::new())
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::click());
                let hovered = resp.hovered();
                let radius = if hovered { 16 } else { 24 };
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(radius),
                    if hovered { t.online } else { t.elevated },
                );
                paint_plus(
                    ui,
                    rect.center(),
                    16.0,
                    if hovered { Color32::WHITE } else { t.online },
                );
                resp.on_hover_text("Drag servers to reorder or drop onto another server to group");
            });
        });

    let layout_snapshot = app.prefs.guild_layout.clone();
    let mut rail_ops: Vec<RailOp> = Vec::new();
    let mut new_drag: Option<RailDrag> = app.view.rail_drag;
    let mut hover_target: Option<RailDropTarget> = None;
    let pointer_pos = ui.ctx().input(|i| i.pointer.interact_pos());

    ui.vertical_centered(|ui| {
        ui.add_space(12.0);
        let home_sel = app.view.guild.is_none();
        let dm_mentions = app.store.dm_mention_count();
        let dm_unread = !home_sel
            && (dm_mentions > 0
                || app
                    .store
                    .private_channels()
                    .iter()
                    .any(|c| app.is_unread(c)));
        if rail_item(
            ui,
            app,
            home_sel,
            dm_unread,
            dm_mentions,
            "Direct Messages",
            "N",
            None,
        )
        .clicked()
        {
            actions.push(Action::SelectHome);
        }
        ui.add_space(4.0);
        let (sep, _) = ui.allocate_exact_size(Vec2::new(32.0, 2.0), Sense::hover());
        ui.painter()
            .rect_filled(sep, CornerRadius::same(1), t.divider);
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("rail_scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let app: &NixApp = app;
                    for entry in &layout_snapshot {
                        if !entry.is_folder() {
                            let Some(&gid) = entry.guild_ids.first() else {
                                continue;
                            };
                            let Some(guild) = app.store.guild(gid).filter(|g| !g.unavailable)
                            else {
                                continue;
                            };
                            let sel = app.view.guild == Some(guild.id);
                            let mentions = app.store.guild_mention_count(guild.id);
                            let has_unread = app.guild_unread(guild.id) || mentions > 0;
                            let unread = !sel && has_unread;
                            let label = initials(&guild.name, 3);
                            let resp = rail_item_draggable(
                                ui,
                                app,
                                sel,
                                unread,
                                mentions,
                                &guild.name,
                                &label,
                                Some(guild),
                            );
                            if resp.drag_started() {
                                new_drag = Some(RailDrag::Guild(guild.id));
                            }
                            if let (Some(drag), Some(pos)) = (new_drag, pointer_pos) {
                                if resp.rect.expand2(Vec2::new(8.0, 4.0)).contains(pos) {
                                    let rel = ((pos.y - resp.rect.top()) / resp.rect.height())
                                        .clamp(0.0, 1.0);
                                    match drag {
                                        RailDrag::Guild(s) if s != guild.id => {
                                            if rel < 0.25 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeTopGuild(guild.id));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.top() - 4.0,
                                                    t.accent,
                                                );
                                            } else if rel > 0.75 {
                                                hover_target =
                                                    Some(RailDropTarget::AfterTopGuild(guild.id));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.bottom() + 4.0,
                                                    t.accent,
                                                );
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::GroupWithGuild(guild.id));
                                                paint_group_ring(ui, resp.rect, t.accent);
                                            }
                                        }
                                        RailDrag::Folder(_) => {
                                            if rel < 0.5 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeTopGuild(guild.id));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.top() - 4.0,
                                                    t.accent,
                                                );
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::AfterTopGuild(guild.id));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.bottom() + 4.0,
                                                    t.accent,
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if resp.clicked() {
                                actions.push(Action::SelectGuild(guild.id));
                            }
                            let items =
                                guild_rail_context_items(guild.id, has_unread, &layout_snapshot);
                            show_context_menu_full(&resp, t, &items, actions, &mut rail_ops);
                            ui.add_space(8.0);
                            continue;
                        }

                        // Server folder (collapsed or expanded).
                        let fid = entry.id.unwrap_or(1);
                        let f_color = entry.color.map(rgb_u32).unwrap_or(t.accent);
                        let is_collapsed = app.prefs.collapsed_folders.contains(&fid);
                        let f_guilds: Vec<&Guild> = entry
                            .guild_ids
                            .iter()
                            .filter_map(|id| app.store.guild(*id))
                            .filter(|g| !g.unavailable)
                            .collect();
                        if f_guilds.is_empty() {
                            continue;
                        }
                        let f_mentions: u32 = f_guilds
                            .iter()
                            .map(|g| app.store.guild_mention_count(g.id))
                            .sum();
                        let f_has_unread =
                            f_mentions > 0 || f_guilds.iter().any(|g| app.guild_unread(g.id));
                        let f_any_selected = f_guilds.iter().any(|g| app.view.guild == Some(g.id));
                        let f_tooltip = format!(
                            "{} ({} {})",
                            entry.display_name(),
                            f_guilds.len(),
                            if f_guilds.len() == 1 {
                                "server"
                            } else {
                                "servers"
                            }
                        );

                        if is_collapsed {
                            let resp = collapsed_folder_item(
                                ui,
                                app,
                                fid,
                                f_color,
                                f_any_selected,
                                !f_any_selected && f_has_unread,
                                f_mentions,
                                &f_tooltip,
                                &f_guilds,
                            );
                            if resp.drag_started() {
                                new_drag = Some(RailDrag::Folder(fid));
                            }
                            if let (Some(drag), Some(pos)) = (new_drag, pointer_pos) {
                                if resp.rect.expand2(Vec2::new(8.0, 4.0)).contains(pos) {
                                    let rel = ((pos.y - resp.rect.top()) / resp.rect.height())
                                        .clamp(0.0, 1.0);
                                    match drag {
                                        RailDrag::Guild(_) => {
                                            if rel < 0.24 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeFolder(fid));
                                                paint_drop_line(ui, resp.rect.top() - 4.0, f_color);
                                            } else if rel > 0.76 {
                                                hover_target =
                                                    Some(RailDropTarget::AfterFolder(fid));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.bottom() + 4.0,
                                                    f_color,
                                                );
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::IntoFolder(fid));
                                                paint_group_ring(ui, resp.rect, f_color);
                                            }
                                        }
                                        RailDrag::Folder(sfid) if sfid != fid => {
                                            if rel < 0.5 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeFolder(fid));
                                                paint_drop_line(ui, resp.rect.top() - 4.0, f_color);
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::AfterFolder(fid));
                                                paint_drop_line(
                                                    ui,
                                                    resp.rect.bottom() + 4.0,
                                                    f_color,
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if resp.clicked() {
                                rail_ops.push(RailOp::ToggleFolder(fid));
                            }
                            let items = folder_context_items(fid, f_has_unread, &layout_snapshot);
                            show_context_menu_full(&resp, t, &items, actions, &mut rail_ops);
                            ui.add_space(8.0);
                        } else {
                            // Expanded folder: header + member guilds enclosed in a vertical pill.
                            let pill_h = 48.0 + (f_guilds.len() as f32) * 54.0 + 4.0;
                            let pill_top = ui.cursor().top();
                            let pill_center_x = ui.max_rect().center().x;
                            let pill_rect = Rect::from_min_size(
                                Pos2::new(pill_center_x - 24.0, pill_top),
                                Vec2::new(48.0, pill_h),
                            );
                            let pill_fill = super::presence::over(
                                f_color.gamma_multiply(0.18),
                                t.elevated.gamma_multiply(0.85),
                            );
                            ui.painter()
                                .rect_filled(pill_rect, CornerRadius::same(16), pill_fill);

                            let hdr_resp =
                                expanded_folder_header(ui, app, fid, f_color, &f_tooltip);
                            if hdr_resp.drag_started() {
                                new_drag = Some(RailDrag::Folder(fid));
                            }
                            if let (Some(drag), Some(pos)) = (new_drag, pointer_pos) {
                                if hdr_resp.rect.expand2(Vec2::new(8.0, 4.0)).contains(pos) {
                                    let rel = ((pos.y - hdr_resp.rect.top())
                                        / hdr_resp.rect.height())
                                    .clamp(0.0, 1.0);
                                    match drag {
                                        RailDrag::Guild(_) => {
                                            if rel < 0.28 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeFolder(fid));
                                                paint_drop_line(
                                                    ui,
                                                    hdr_resp.rect.top() - 4.0,
                                                    f_color,
                                                );
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::IntoFolder(fid));
                                                paint_group_ring(ui, hdr_resp.rect, f_color);
                                            }
                                        }
                                        RailDrag::Folder(sfid) if sfid != fid => {
                                            if rel < 0.5 {
                                                hover_target =
                                                    Some(RailDropTarget::BeforeFolder(fid));
                                                paint_drop_line(
                                                    ui,
                                                    hdr_resp.rect.top() - 4.0,
                                                    f_color,
                                                );
                                            } else {
                                                hover_target =
                                                    Some(RailDropTarget::AfterFolder(fid));
                                                paint_drop_line(
                                                    ui,
                                                    pill_rect.bottom() + 4.0,
                                                    f_color,
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if hdr_resp.clicked() {
                                rail_ops.push(RailOp::ToggleFolder(fid));
                            }
                            let f_items = folder_context_items(fid, f_has_unread, &layout_snapshot);
                            show_context_menu_full(&hdr_resp, t, &f_items, actions, &mut rail_ops);

                            ui.add_space(6.0);
                            for (g_idx, guild) in f_guilds.iter().enumerate() {
                                let sel = app.view.guild == Some(guild.id);
                                let mentions = app.store.guild_mention_count(guild.id);
                                let has_unread = app.guild_unread(guild.id) || mentions > 0;
                                let unread = !sel && has_unread;
                                let label = initials(&guild.name, 3);
                                let resp = rail_item_draggable(
                                    ui,
                                    app,
                                    sel,
                                    unread,
                                    mentions,
                                    &guild.name,
                                    &label,
                                    Some(guild),
                                );
                                if resp.drag_started() {
                                    new_drag = Some(RailDrag::Guild(guild.id));
                                }
                                if let (Some(RailDrag::Guild(s)), Some(pos)) =
                                    (new_drag, pointer_pos)
                                {
                                    if s != guild.id
                                        && resp.rect.expand2(Vec2::new(8.0, 3.0)).contains(pos)
                                    {
                                        let rel = ((pos.y - resp.rect.top()) / resp.rect.height())
                                            .clamp(0.0, 1.0);
                                        if rel < 0.5 {
                                            hover_target =
                                                Some(RailDropTarget::InsideFolderBefore {
                                                    folder_id: fid,
                                                    target_guild: guild.id,
                                                });
                                            paint_drop_line(ui, resp.rect.top() - 3.0, f_color);
                                        } else {
                                            hover_target =
                                                Some(RailDropTarget::InsideFolderAfter {
                                                    folder_id: fid,
                                                    target_guild: guild.id,
                                                });
                                            paint_drop_line(ui, resp.rect.bottom() + 3.0, f_color);
                                        }
                                    }
                                }
                                if resp.clicked() {
                                    actions.push(Action::SelectGuild(guild.id));
                                }
                                let items = guild_rail_context_items(
                                    guild.id,
                                    has_unread,
                                    &layout_snapshot,
                                );
                                show_context_menu_full(&resp, t, &items, actions, &mut rail_ops);
                                if g_idx + 1 < f_guilds.len() {
                                    ui.add_space(6.0);
                                } else {
                                    ui.add_space(4.0);
                                }
                            }
                            ui.add_space(8.0);
                        }
                    }
                });
            });
    });

    // Floating drag indicator & drop completion.
    if let Some(drag) = new_drag {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        if let Some(pos) = pointer_pos {
            let layer = egui::LayerId::new(Order::Tooltip, Id::unique("rail_drag_ghost"));
            let p = ui.ctx().layer_painter(layer);
            let badge = Rect::from_center_size(pos + Vec2::new(20.0, 16.0), Vec2::splat(32.0));
            p.rect_filled(badge, CornerRadius::same(10), t.accent.gamma_multiply(0.88));
            let label = match drag {
                RailDrag::Guild(gid) => app
                    .store
                    .guild(gid)
                    .map(|g| initials(&g.name, 2))
                    .unwrap_or_else(|| "S".into()),
                RailDrag::Folder(_) => "📁".into(),
            };
            p.text(
                badge.center(),
                Align2::CENTER_CENTER,
                label,
                bold(12.0),
                t.on_accent,
            );
        }
        let released = ui.ctx().input(|i| i.pointer.any_released());
        let escaped = ui.ctx().input(|i| i.key_pressed(Key::Escape));
        if escaped {
            new_drag = None;
        } else if released {
            if let Some(target) = hover_target {
                rail_ops.push(RailOp::Drop {
                    source: drag,
                    target,
                });
            }
            new_drag = None;
        }
    }
    app.view.rail_drag = new_drag;

    for op in rail_ops {
        if let RailOp::MarkFolderRead(fid) = &op {
            if let Some(f) = app.prefs.guild_layout.iter().find(|f| f.id == Some(*fid)) {
                for &gid in &f.guild_ids {
                    actions.push(Action::MarkGuildRead(gid));
                }
            }
        }
        if let Some(edit_fid) = apply_rail_op(
            &mut app.prefs.guild_layout,
            &mut app.prefs.collapsed_folders,
            op,
        ) {
            if let Some(folder) = app
                .prefs
                .guild_layout
                .iter()
                .find(|f| f.id == Some(edit_fid))
            {
                app.view.folder_modal = Some(FolderModal {
                    folder_id: edit_fid,
                    name: folder.name.clone().unwrap_or_default(),
                    color: folder.color,
                    focus_name: true,
                });
            }
        }
    }
}

fn paint_drop_line(ui: &Ui, y: f32, color: Color32) {
    let cx = ui.max_rect().center().x;
    let bar = Rect::from_center_size(Pos2::new(cx, y), Vec2::new(42.0, 3.5));
    ui.painter().rect_filled(bar, CornerRadius::same(2), color);
}

fn paint_group_ring(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().rect_stroke(
        rect.expand(2.5),
        CornerRadius::same(16),
        Stroke::new(2.2, color),
        StrokeKind::Outside,
    );
}

fn paint_folder_icon(ui: &Ui, center: Pos2, size: f32, color: Color32) {
    let p = ui.painter();
    let w = size;
    let h = size * 0.78;
    let body = Rect::from_center_size(center + Vec2::new(0.0, size * 0.06), Vec2::new(w, h));
    let tab = Rect::from_min_size(
        Pos2::new(body.left(), body.top() - size * 0.14),
        Vec2::new(w * 0.44, size * 0.22),
    );
    p.rect_filled(tab, CornerRadius::same(3), color);
    p.rect_filled(body, CornerRadius::same(4), color);
}

fn expanded_folder_header(
    ui: &mut Ui,
    app: &NixApp,
    folder_id: u64,
    folder_color: Color32,
    tooltip: &str,
) -> egui::Response {
    let t = app.theme;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::click_and_drag());
    let _ = folder_id;
    if resp.hovered() {
        ui.painter().rect_filled(
            rect.shrink(4.0),
            CornerRadius::same(12),
            t.hover.gamma_multiply(0.75),
        );
    }
    paint_folder_icon(ui, rect.center(), 20.0, folder_color);
    resp.on_hover_text(tooltip)
}

#[allow(clippy::too_many_arguments)]
fn collapsed_folder_item(
    ui: &mut Ui,
    app: &NixApp,
    folder_id: u64,
    folder_color: Color32,
    selected: bool,
    unread: bool,
    mentions: u32,
    tooltip: &str,
    guilds: &[&Guild],
) -> egui::Response {
    let t = app.theme;
    let _ = folder_id;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::click_and_drag());
    let hovered = resp.hovered();
    let bg = super::presence::over(
        folder_color.gamma_multiply(if hovered { 0.38 } else { 0.26 }),
        t.elevated,
    );
    ui.painter().rect_filled(rect, CornerRadius::same(16), bg);

    // 2x2 mini-grid of up to 4 servers inside the folder.
    let offsets = [
        Vec2::new(-10.0, -10.0),
        Vec2::new(10.0, -10.0),
        Vec2::new(-10.0, 10.0),
        Vec2::new(10.0, 10.0),
    ];
    for (g, off) in guilds.iter().take(4).zip(offsets) {
        let mini = Rect::from_center_size(rect.center() + off, Vec2::splat(16.0));
        let mini_round = CornerRadius::same(8);
        let has_icon = app
            .media
            .borrow_mut()
            .paint_guild_icon(ui, mini, g, mini_round);
        if !has_icon {
            ui.painter().rect_filled(mini, mini_round, id_color(g.id));
            ui.painter().text(
                mini.center(),
                Align2::CENTER_CENTER,
                initials(&g.name, 1),
                bold(9.5),
                Color32::WHITE,
            );
        }
    }

    if mentions > 0 {
        let ring = super::presence::opaque_bg(t, t.rail);
        paint_mention_badge(
            ui,
            Pos2::new(rect.right() + 2.0, rect.bottom() - 7.0),
            mentions,
            Some(ring),
            t,
        );
    }
    let pill_h = if selected {
        40.0
    } else if hovered {
        20.0
    } else if unread {
        8.0
    } else {
        0.0
    };
    if pill_h > 0.0 {
        let left = rect.left() - 12.0;
        let pill = Rect::from_center_size(Pos2::new(left, rect.center().y), Vec2::new(8.0, pill_h));
        ui.painter()
            .rect_filled(pill, CornerRadius::same(4), t.strong);
    }
    resp.on_hover_text(tooltip)
}

/// Modal dialog for renaming and recoloring a server folder.
pub fn folder_modal(ctx: &egui::Context, app: &mut NixApp, _actions: &mut Vec<Action>) {
    let Some(mut state) = app.view.folder_modal.take() else {
        return;
    };
    let t = app.theme;
    let mut keep_open = true;
    let mut save_clicked = false;
    let mut ungroup_clicked = false;

    let modal = Modal::new(Id::unique("folder_settings_modal")).show(ctx, |ui| {
        ui.set_width(340.0);
        Frame::new()
            .fill(t.sidebar)
            .stroke(Stroke::new(1.0, t.divider))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Folder Settings")
                        .font(bold(17.5))
                        .color(t.strong),
                );
                ui.add_space(12.0);
                ui.label(RichText::new("FOLDER NAME").font(bold(11.5)).color(t.muted));
                ui.add_space(4.0);
                let te_resp = Frame::new()
                    .fill(t.elevated)
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.add(
                            TextEdit::singleline(&mut state.name)
                                .hint_text("Server Folder")
                                .char_limit(32)
                                .frame(Frame::NONE)
                                .font(prop(14.5))
                                .desired_width(f32::INFINITY),
                        )
                    })
                    .inner;
                if std::mem::take(&mut state.focus_name) {
                    te_resp.request_focus();
                }
                if te_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    save_clicked = true;
                }

                ui.add_space(14.0);
                ui.label(
                    RichText::new("FOLDER COLOR")
                        .font(bold(11.5))
                        .color(t.muted),
                );
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(8.0);
                    for &(swatch, label) in FOLDER_SWATCHES {
                        let col = swatch.map(rgb_u32).unwrap_or(t.accent);
                        let active = state.color == swatch;
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 12.0, col);
                        if active {
                            ui.painter().circle_stroke(
                                r.center(),
                                13.5,
                                Stroke::new(2.0, t.strong),
                            );
                        }
                        if resp
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .on_hover_text(label)
                            .clicked()
                        {
                            state.color = swatch;
                        }
                    }
                });

                ui.add_space(18.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), 32.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        let save_btn = ui.add(
                            egui::Button::new(
                                RichText::new("Save").font(bold(13.5)).color(t.on_accent),
                            )
                            .fill(t.accent)
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(72.0, 32.0)),
                        );
                        if save_btn.clicked() {
                            save_clicked = true;
                        }
                        let cancel_btn = ui.add(
                            egui::Button::new(
                                RichText::new("Cancel").font(prop(13.5)).color(t.text),
                            )
                            .fill(t.elevated)
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(72.0, 32.0)),
                        );
                        if cancel_btn.clicked() {
                            keep_open = false;
                        }
                        let ungroup_btn = ui.add(
                            egui::Button::new(
                                RichText::new("Ungroup").font(prop(13.5)).color(t.danger),
                            )
                            .fill(t.elevated)
                            .corner_radius(CornerRadius::same(6))
                            .min_size(Vec2::new(78.0, 32.0)),
                        );
                        if ungroup_btn.clicked() {
                            ungroup_clicked = true;
                        }
                    },
                );
            });
    });

    if modal.should_close() {
        keep_open = false;
    }

    if ungroup_clicked {
        apply_rail_op(
            &mut app.prefs.guild_layout,
            &mut app.prefs.collapsed_folders,
            RailOp::UngroupFolder(state.folder_id),
        );
    } else if save_clicked {
        if let Some(folder) = app
            .prefs
            .guild_layout
            .iter_mut()
            .find(|f| f.id == Some(state.folder_id))
        {
            let trimmed = state.name.trim();
            folder.name = (!trimmed.is_empty()).then(|| trimmed.to_string());
            folder.color = state.color;
        }
    } else if keep_open {
        app.view.folder_modal = Some(state);
    }
}

/// One 48px rail icon (server icon, or initials on a circle) that rounds into a squircle
/// when active, with the left-edge pill (selected / hovered / unread) and optional red
/// mention badge at the bottom right.
#[allow(clippy::too_many_arguments)]
fn rail_item(
    ui: &mut Ui,
    app: &NixApp,
    selected: bool,
    unread: bool,
    mentions: u32,
    tooltip: &str,
    text: &str,
    guild: Option<&Guild>,
) -> egui::Response {
    rail_item_with_sense(
        ui,
        app,
        selected,
        unread,
        mentions,
        tooltip,
        text,
        guild,
        Sense::click(),
    )
}

#[allow(clippy::too_many_arguments)]
fn rail_item_draggable(
    ui: &mut Ui,
    app: &NixApp,
    selected: bool,
    unread: bool,
    mentions: u32,
    tooltip: &str,
    text: &str,
    guild: Option<&Guild>,
) -> egui::Response {
    rail_item_with_sense(
        ui,
        app,
        selected,
        unread,
        mentions,
        tooltip,
        text,
        guild,
        Sense::click_and_drag(),
    )
}

#[allow(clippy::too_many_arguments)]
fn rail_item_with_sense(
    ui: &mut Ui,
    app: &NixApp,
    selected: bool,
    unread: bool,
    mentions: u32,
    tooltip: &str,
    text: &str,
    guild: Option<&Guild>,
    sense: Sense,
) -> egui::Response {
    let t = app.theme;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), sense);
    let hovered = resp.hovered();
    // Always a rounded square, like the current official client (no circle morph).
    let rounding = CornerRadius::same(RAIL_ICON_RADIUS);
    let has_icon = guild.is_some_and(|g| {
        app.media
            .borrow_mut()
            .paint_guild_icon(ui, rect, g, rounding)
    });
    if !has_icon {
        let (fill, fg) = if selected {
            (t.accent, t.on_accent)
        } else if hovered {
            (t.accent.gamma_multiply(0.85), t.on_accent)
        } else {
            (t.elevated, t.text)
        };
        ui.painter().rect_filled(rect, rounding, fill);
        let size = match text.chars().count() {
            0 | 1 => 20.0,
            2 => 17.0,
            _ => 14.0,
        };
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, text, bold(size), fg);
    }
    if mentions > 0 {
        let ring = super::presence::opaque_bg(t, t.rail);
        paint_mention_badge(
            ui,
            Pos2::new(rect.right() + 2.0, rect.bottom() - 7.0),
            mentions,
            Some(ring),
            t,
        );
    }
    // Pill on the panel's left edge.
    let pill_h = if selected {
        40.0
    } else if hovered {
        20.0
    } else if unread {
        8.0
    } else {
        0.0
    };
    if pill_h > 0.0 {
        let left = rect.left() - 12.0;
        let pill = Rect::from_center_size(Pos2::new(left, rect.center().y), Vec2::new(8.0, pill_h));
        ui.painter()
            .rect_filled(pill, CornerRadius::same(4), t.strong);
    }
    resp.on_hover_text(tooltip)
}

// ---- channel column -----------------------------------------------------------------

pub fn column(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Hairline between the channel column and the chat.
    let edge = ui.max_rect().right() - 0.5;
    ui.painter().vline(
        edge,
        ui.max_rect().y_range(),
        egui::Stroke::new(1.0, t.divider),
    );
    // Room for the floating user card, which spans the rail and this column and is
    // drawn by `user_card` once both panels are laid out.
    egui::Panel::bottom("user_panel")
        .exact_size(USER_CARD_RESERVE)
        .show_separator_line(false)
        .frame(Frame::new())
        .show(ui, |_| {});

    if app.view.voice.is_active() {
        egui::Panel::bottom("voice_connected_bar")
            .exact_size(52.0)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin {
                left: 6,
                right: 6,
                top: 4,
                bottom: 0,
            }))
            .show(ui, |ui| super::voice::connected_bar(ui, app, actions));
    }

    // Header (48px).
    let header_rect =
        Rect::from_min_size(ui.max_rect().min, Vec2::new(ui.max_rect().width(), 48.0));
    ui.allocate_rect(header_rect, Sense::hover());
    super::hline(ui, header_rect, header_rect.bottom() - 0.5, t.divider);

    match app.view.guild {
        Some(gid) => {
            let name = app
                .store
                .guild(gid)
                .map_or_else(|| "Unknown server".to_string(), |g| g.name.clone());
            let inner = header_rect.shrink2(Vec2::new(16.0, 0.0));
            let resp = ui.interact(
                header_rect,
                ui.scope_id().with("guild_header"),
                Sense::click(),
            );
            if resp.hovered() {
                ui.painter().rect_filled(header_rect, 0.0, t.hover);
            }
            let p = ui.painter().with_clip_rect(inner);
            let galley = emoji_text::layout(&p, &name, bold(15.5), t.strong, f32::INFINITY, 0);
            let w = galley.size().x.min(inner.width() - 24.0);
            galley.paint(
                &p,
                Pos2::new(inner.left(), inner.center().y - galley.size().y / 2.0),
                &app.media,
            );
            paint_chevron(
                ui,
                Pos2::new(inner.left() + w + 12.0, inner.center().y),
                8.0,
                true,
                t.strong,
            );
        }
        None => {
            let field = header_rect.shrink2(Vec2::new(10.0, 10.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
            Frame::new()
                .fill(t.elevated)
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::symmetric(8, 3))
                .show(&mut child, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.view.dm_filter)
                                .hint_text("Find or start a conversation")
                                .frame(Frame::NONE)
                                .font(prop(13.5))
                                .desired_width(ui.available_width() - 18.0),
                        );
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        paint_search(ui, r.center(), 14.0, t.muted);
                    });
                });
        }
    }

    egui::ScrollArea::vertical()
        .id_salt(("channel_list", app.view.guild))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            match app.view.guild {
                Some(gid) => guild_channels(ui, app, gid, actions),
                None => dm_list(ui, app, actions),
            }
            ui.add_space(8.0);
        });
}

fn is_voice_channel(kind: ChannelKind) -> bool {
    matches!(kind, ChannelKind::Voice | ChannelKind::Other(13))
}

fn channel_glyph(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Voice | ChannelKind::Other(13) => "🔊",
        ChannelKind::Announcement => "📢",
        _ => "#",
    }
}

/// Corner radius of the 48 px server icons in the rail.
const RAIL_ICON_RADIUS: u8 = 14;

/// The forum channel icon: two overlapping filled speech bubbles.
fn paint_forum_icon(ui: &Ui, center: Pos2, color: Color32, bg: Color32) {
    let p = ui.painter();
    let bubble = |c: Pos2, w: f32, h: f32, fill: Color32, tail_left: bool| {
        let r = Rect::from_center_size(c, Vec2::new(w, h));
        p.rect_filled(r, CornerRadius::same(3), fill);
        let x = if tail_left {
            r.left() + 2.0
        } else {
            r.right() - 2.0
        };
        let dx = if tail_left { 4.0 } else { -4.0 };
        p.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x, r.bottom() - 1.0),
                Pos2::new(x + dx, r.bottom() - 1.0),
                Pos2::new(x, r.bottom() + 3.0),
            ],
            fill,
            egui::Stroke::NONE,
        ));
    };
    // Back bubble, then a cut-out gap, then the front bubble.
    bubble(center + Vec2::new(3.0, -3.0), 12.0, 9.0, color, false);
    bubble(center + Vec2::new(-2.0, 1.5), 13.0, 10.0, bg, true);
    bubble(center + Vec2::new(-2.5, 2.0), 12.0, 9.0, color, true);
}

/// Grey count pill on a forum row (new posts since it was last read), right-aligned at
/// `right_center`. Sized like the mention badge so `badge_reserve` fits both.
fn paint_count_badge(ui: &Ui, right_center: Pos2, count: u32, t: &Theme) {
    let g = ui
        .painter()
        .layout_no_wrap(badge_text(count), bold(11.0), t.text);
    let w = (g.size().x + 8.0).max(16.0);
    let rect = Rect::from_min_size(
        Pos2::new(right_center.x - w, right_center.y - 8.0),
        Vec2::new(w, 16.0),
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), t.elevated);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, t.text);
}

fn guild_channels(ui: &mut Ui, app: &NixApp, gid: Snowflake, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Hides channels I can't view, "__hidden__" placeholders and emptied categories.
    let channels = app.store.visible_guild_channels(gid);
    let mut current_cat: Option<Snowflake> = None;
    for ch in channels {
        if ch.kind == ChannelKind::Category {
            current_cat = Some(ch.id);
            let collapsed = app.view.collapsed.contains(&ch.id);
            ui.add_space(12.0);
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
            let color = if resp.hovered() { t.text } else { t.muted };
            let label = ch.label();
            let galley =
                emoji_text::layout(ui.painter(), &label, prop(14.0), color, f32::INFINITY, 0);
            let text_pos = Pos2::new(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
            let chev_x = text_pos.x + galley.size().x + 9.0;
            galley.paint(ui.painter(), text_pos, &app.media);
            paint_chevron(
                ui,
                Pos2::new(chev_x, rect.center().y),
                7.0,
                !collapsed,
                color,
            );
            if resp.clicked() {
                actions.push(Action::ToggleCategory(ch.id));
            }
            continue;
        }
        if ch.kind.is_thread() {
            continue;
        }
        if ch.parent_id.is_none() {
            current_cat = None;
        }
        let threads = app.store.channel_threads(ch.id);
        let collapsed_cat = current_cat.is_some_and(|c| app.view.collapsed.contains(&c));
        let active_in_branch = app.view.channel == Some(ch.id)
            || app.view.voice.channel_id == Some(ch.id)
            || threads.iter().any(|th| app.view.channel == Some(th.id));
        let hidden = collapsed_cat && !active_in_branch;
        if hidden {
            continue;
        }
        channel_row(ui, app, ch, actions);
        if is_voice_channel(ch.kind) {
            super::voice::participants(ui, app, ch.id, actions);
        }
        // Like Discord: only threads you've joined (or have open) are listed.
        let visible_threads: Vec<&Channel> = threads
            .into_iter()
            .filter(|th| {
                let open = app.view.channel == Some(th.id);
                open || (!collapsed_cat && app.store.thread_joined(th.id))
            })
            .collect();
        let n = visible_threads.len();
        for (i, th) in visible_threads.into_iter().enumerate() {
            thread_row(ui, app, th, i + 1 == n, actions);
        }
    }
}

fn channel_row(ui: &mut Ui, app: &NixApp, ch: &Channel, actions: &mut Vec<Action>) {
    let t = app.theme;
    let openable = ch.kind.is_openable();
    let is_voice = is_voice_channel(ch.kind);
    let clickable = openable || is_voice;
    let is_forum = ch.kind == ChannelKind::Forum;
    let selected =
        app.view.channel == Some(ch.id) || (is_voice && app.view.voice.channel_id == Some(ch.id));
    let row_w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(row_w, 32.0), Sense::click());
    let rect = outer.shrink2(Vec2::new(8.0, 1.0));
    let hovered = resp.hovered() && clickable;
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.selected);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let (raw_mentions, raw_unread) = if !openable {
        (0, false)
    } else if is_forum {
        let threads = app.store.channel_threads(ch.id);
        let m: u32 = threads
            .iter()
            .map(|th| app.store.channel_mention_count(th.id))
            .sum();
        let u = m > 0 || threads.iter().any(|th| app.is_unread(th));
        (m, u)
    } else {
        let m = app.store.channel_mention_count(ch.id);
        (m, app.is_unread(ch) || m > 0)
    };
    let (mentions, unread) = if selected {
        (0, false)
    } else {
        (raw_mentions, raw_unread)
    };
    // Forums also show how many posts are new since the forum was last read.
    let new_posts = if is_forum && !selected && mentions == 0 {
        app.store.forum_new_posts(ch.id)
    } else {
        0
    };
    if unread {
        // Small white pill on the column's left edge.
        let dot = Rect::from_center_size(
            Pos2::new(outer.left(), rect.center().y),
            Vec2::new(8.0, 8.0),
        );
        ui.painter()
            .rect_filled(dot, CornerRadius::same(4), t.strong);
    }
    let color = if !clickable {
        t.muted.gamma_multiply(0.55)
    } else if selected || hovered || unread {
        t.strong
    } else {
        t.muted
    };
    let name_font = if unread { bold(15.5) } else { prop(15.5) };
    let icon_center = Pos2::new(rect.left() + 18.0, rect.center().y);
    if is_forum {
        let row_bg = if selected {
            t.selected
        } else if hovered {
            t.hover
        } else {
            t.sidebar
        };
        let bg = super::presence::opaque_bg(t, row_bg);
        paint_forum_icon(ui, icon_center, color, bg);
    } else {
        let glyph = channel_glyph(ch.kind);
        let glyph_font = if glyph == "#" { prop(21.0) } else { prop(15.0) };
        ui.painter()
            .text(icon_center, Align2::CENTER_CENTER, glyph, glyph_font, color);
    }
    let text_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 34.0, rect.top()),
        Pos2::new(
            rect.right() - 8.0 - badge_reserve(mentions.max(new_posts)),
            rect.bottom(),
        ),
    );
    let p = ui
        .painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()));
    let galley = emoji_text::layout(&p, &ch.label(), name_font, color, f32::INFINITY, 0);
    galley.paint(
        &p,
        Pos2::new(
            text_rect.left(),
            text_rect.center().y - galley.size().y / 2.0,
        ),
        &app.media,
    );
    if mentions > 0 {
        paint_mention_badge(
            ui,
            Pos2::new(rect.right() - 6.0, rect.center().y),
            mentions,
            None,
            t,
        );
    } else if new_posts > 0 {
        paint_count_badge(
            ui,
            Pos2::new(rect.right() - 6.0, rect.center().y),
            new_posts,
            t,
        );
    }
    if resp.clicked() {
        if is_voice {
            if app.store.can_connect(ch.id) {
                actions.push(Action::JoinVoice {
                    guild_id: ch.guild_id,
                    channel_id: ch.id,
                });
            } else {
                actions.push(Action::Toast(
                    "You don't have permission to connect to this voice channel.".into(),
                ));
            }
        } else if openable {
            actions.push(Action::SelectChannel(ch.id));
        }
    }
    let items = channel_context_items(ch.id, raw_unread);
    show_context_menu(&resp, t, &items, actions);
    if !clickable {
        resp.on_hover_text("Not supported in nixcord yet");
    }
}

/// Active thread row indented beneath its parent channel, with a Discord-style branch
/// connector spine, unread indicator, and mention badge.
fn thread_row(ui: &mut Ui, app: &NixApp, ch: &Channel, is_last: bool, actions: &mut Vec<Action>) {
    let t = app.theme;
    let selected = app.view.channel == Some(ch.id);
    let row_w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(row_w, 28.0), Sense::click());
    let rect = outer.shrink2(Vec2::new(8.0, 1.0));

    // Branch connector spine on the left.
    let spine_x = rect.left() + 18.0;
    let cy = rect.center().y;
    let spine_stroke = Stroke::new(1.5, t.muted.gamma_multiply(0.45));
    let spine_end_y = if is_last { cy - 3.0 } else { outer.bottom() };
    ui.painter().line_segment(
        [
            Pos2::new(spine_x, outer.top()),
            Pos2::new(spine_x, spine_end_y),
        ],
        spine_stroke,
    );
    let elbow = vec![
        Pos2::new(spine_x, cy - 3.0),
        Pos2::new(spine_x + 1.0, cy - 0.8),
        Pos2::new(spine_x + 3.5, cy),
        Pos2::new(spine_x + 11.0, cy),
    ];
    ui.painter().add(egui::Shape::line(elbow, spine_stroke));

    let pill = Rect::from_min_max(Pos2::new(rect.left() + 32.0, rect.top()), rect.max);
    let hovered = resp.hovered();
    if selected {
        ui.painter()
            .rect_filled(pill, CornerRadius::same(4), t.selected);
    } else if hovered {
        ui.painter()
            .rect_filled(pill, CornerRadius::same(4), t.hover);
    }

    let raw_mentions = app.store.channel_mention_count(ch.id);
    let raw_unread = app.is_unread(ch) || raw_mentions > 0;
    let mentions = if !selected { raw_mentions } else { 0 };
    let unread = !selected && raw_unread;
    if unread {
        let dot = Rect::from_center_size(Pos2::new(outer.left(), cy), Vec2::new(8.0, 8.0));
        ui.painter()
            .rect_filled(dot, CornerRadius::same(4), t.strong);
    }
    let color = if selected || hovered || unread {
        t.strong
    } else {
        t.muted
    };
    let name_font = if unread { bold(14.0) } else { prop(14.0) };
    let text_rect = Rect::from_min_max(
        Pos2::new(pill.left() + 8.0, pill.top()),
        Pos2::new(pill.right() - 6.0 - badge_reserve(mentions), pill.bottom()),
    );
    let p = ui
        .painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()));
    let galley = emoji_text::layout(&p, &ch.label(), name_font, color, f32::INFINITY, 0);
    galley.paint(
        &p,
        Pos2::new(text_rect.left(), cy - galley.size().y / 2.0),
        &app.media,
    );
    if mentions > 0 {
        paint_mention_badge(ui, Pos2::new(pill.right() - 6.0, cy), mentions, None, t);
    }
    if resp.clicked() {
        actions.push(Action::SelectChannel(ch.id));
    }
    let is_archived = ch.is_archived();
    let is_locked = ch.thread_metadata.as_ref().is_some_and(|m| m.locked);
    let items = thread_context_items(ch.id, raw_unread, is_archived, is_locked);
    show_context_menu(&resp, t, &items, actions);
}

fn dm_list(ui: &mut Ui, app: &NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Top "Friends" navigation item (active when on Home with no DM selected).
    let friends_sel = app.view.guild.is_none() && app.view.channel.is_none();
    let incoming = app.store.incoming_request_count();
    let (f_outer, f_resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
    let f_rect = f_outer.shrink2(Vec2::new(8.0, 1.0));
    if friends_sel {
        ui.painter()
            .rect_filled(f_rect, CornerRadius::same(4), t.selected);
    } else if f_resp.hovered() {
        ui.painter()
            .rect_filled(f_rect, CornerRadius::same(4), t.hover);
    }
    let f_color = if friends_sel || f_resp.hovered() {
        t.strong
    } else {
        t.muted
    };
    let icon_rect = Rect::from_center_size(
        Pos2::new(f_rect.left() + 22.0, f_rect.center().y),
        Vec2::splat(22.0),
    );
    super::paint_icon(ui, Icon::Members, icon_rect, f_color);
    ui.painter().text(
        Pos2::new(f_rect.left() + 44.0, f_rect.center().y),
        Align2::LEFT_CENTER,
        "Friends",
        if friends_sel { bold(15.5) } else { prop(15.5) },
        f_color,
    );
    if incoming > 0 {
        paint_mention_badge(
            ui,
            Pos2::new(f_rect.right() - 6.0, f_rect.center().y),
            incoming,
            None,
            t,
        );
    }
    if f_resp.clicked() {
        actions.push(Action::OpenFriends);
    }
    ui.add_space(10.0);

    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(
            RichText::new("Direct Messages")
                .font(prop(13.5))
                .color(t.muted),
        );
    });
    ui.add_space(4.0);
    let filter = app.view.dm_filter.trim().to_lowercase();
    let me = app.store.me().map(|u| u.id);
    for ch in app.store.private_channels() {
        let label = ch.label();
        if !filter.is_empty() && !label.to_lowercase().contains(&filter) {
            continue;
        }
        let selected = app.view.channel == Some(ch.id);
        let (outer, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
        let rect = outer.shrink2(Vec2::new(8.0, 1.0));
        let hovered = resp.hovered();
        if selected {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), t.selected);
        } else if hovered {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), t.hover);
        }
        let center = Pos2::new(rect.left() + 8.0 + 16.0, rect.center().y);
        let other = ch
            .recipients
            .iter()
            .find(|u| Some(u.id) != me)
            .or(ch.recipients.first());
        match (ch.kind, other) {
            (ChannelKind::Dm, Some(u)) => {
                app.media
                    .borrow_mut()
                    .paint_avatar(ui, center, 32.0, u, None);
                let panel = super::presence::opaque_bg(t, t.sidebar);
                let ring = if selected {
                    super::presence::over(t.selected, panel)
                } else if hovered {
                    super::presence::over(t.hover, panel)
                } else {
                    panel
                };
                super::presence::paint_user_status(ui, app, u.id, center, 32.0, ring);
            }
            _ => {
                ui.painter()
                    .circle_filled(center, 16.0, super::id_color(ch.id));
                ui.painter().text(
                    center,
                    Align2::CENTER_CENTER,
                    "👥",
                    prop(14.0),
                    Color32::WHITE,
                );
            }
        }
        let mentions = if !selected {
            app.store.channel_mention_count(ch.id)
        } else {
            0
        };
        let unread = !selected && (app.is_unread(ch) || mentions > 0);
        if unread {
            let dot = Rect::from_center_size(
                Pos2::new(outer.left(), rect.center().y),
                Vec2::new(8.0, 8.0),
            );
            ui.painter()
                .rect_filled(dot, CornerRadius::same(4), t.strong);
        }
        let color = if selected || hovered || unread {
            t.strong
        } else {
            t.muted
        };
        let text_left = rect.left() + 52.0;
        let clip = Rect::from_min_max(
            Pos2::new(text_left, rect.top()),
            Pos2::new(rect.right() - 6.0 - badge_reserve(mentions), rect.bottom()),
        )
        .intersect(ui.clip_rect());
        let name_font = if unread { bold(15.5) } else { prop(15.5) };
        let p = ui.painter().with_clip_rect(clip);
        let name = emoji_text::layout(&p, &label, name_font, color, f32::INFINITY, 0);
        // Second line: member count for groups, custom status / activity for DMs.
        let sub_text = match (ch.kind, other) {
            (ChannelKind::GroupDm, _) => Some(format!("{} Members", ch.recipients.len() + 1)),
            (ChannelKind::Dm, Some(u)) => app.store.status_text(u.id),
            _ => None,
        };
        if let Some(sub_text) = sub_text {
            let sub = emoji_text::layout(&p, &sub_text, prop(12.0), t.muted, f32::INFINITY, 0);
            let top = rect.center().y - (name.size().y + sub.size().y) / 2.0;
            let name_h = name.size().y;
            name.paint(&p, Pos2::new(text_left, top), &app.media);
            sub.paint(&p, Pos2::new(text_left, top + name_h), &app.media);
        } else {
            name.paint(
                &p,
                Pos2::new(text_left, rect.center().y - name.size().y / 2.0),
                &app.media,
            );
        }
        if mentions > 0 {
            paint_mention_badge(
                ui,
                Pos2::new(rect.right() - 6.0, rect.center().y),
                mentions,
                None,
                t,
            );
        }
        if resp.clicked() {
            actions.push(Action::SelectChannel(ch.id));
        } else if resp.secondary_clicked() {
            if let (ChannelKind::Dm, Some(u)) = (ch.kind, other) {
                actions.push(Action::OpenProfile {
                    user_id: u.id,
                    guild_id: None,
                    anchor: rect,
                });
            }
        }
    }
}

// ---- user panel ---------------------------------------------------------------------

/// Height of the user card and the space kept free for it under the rail and the column.
const USER_CARD_H: f32 = 56.0;
const USER_CARD_MARGIN: f32 = 8.0;
const USER_CARD_RESERVE: f32 = USER_CARD_H + 2.0 * USER_CARD_MARGIN;

/// The user card: one floating rounded panel across the bottom of the server rail and
/// the channel column, as in the official client. `left_bottom` is the window's
/// bottom-left corner and `width` the rail plus the channel column.
pub fn user_card(
    ui: &mut Ui,
    app: &mut NixApp,
    left_bottom: Pos2,
    width: f32,
    actions: &mut Vec<Action>,
) {
    let rect = Rect::from_min_max(
        Pos2::new(
            left_bottom.x + USER_CARD_MARGIN,
            left_bottom.y - USER_CARD_MARGIN - USER_CARD_H,
        ),
        Pos2::new(
            left_bottom.x + width - USER_CARD_MARGIN,
            left_bottom.y - USER_CARD_MARGIN,
        ),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(6.0))
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.set_clip_rect(rect);
    user_panel(&mut child, app, rect, actions);
}

fn user_card_fill(t: &Theme) -> Color32 {
    super::presence::opaque_bg(t, t.elevated)
}

fn user_panel(ui: &mut Ui, app: &mut NixApp, card: Rect, actions: &mut Vec<Action>) {
    let t = app.theme;
    ui.painter()
        .rect_filled(card, CornerRadius::same(8), user_card_fill(t));
    ui.painter().rect_stroke(
        card,
        CornerRadius::same(8),
        egui::Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    let Some(me) = app.store.me().cloned() else {
        ui.centered_and_justified(|ui| ui.add(egui::Spinner::new().color(t.muted)));
        return;
    };
    // Connected: my own status or custom status (clickable to change it). Otherwise the connection state.
    let my = app.store.my_status();
    let custom_sub = app.store.my_custom_status_text();
    let (status, dot) = match app.store.connection() {
        ConnectionState::Connected => (user_panel_subtitle(custom_sub.as_deref(), my), Some(my)),
        ConnectionState::Connecting => ("Connecting…", None),
        ConnectionState::Reconnecting => ("Reconnecting…", None),
        ConnectionState::Offline => ("Offline", None),
    };
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(2.0);
        if icon_button(ui, t, Icon::Gear, 32.0, "User Settings").clicked() {
            actions.push(Action::OpenSettings);
        }
        let deaf = icon_button(
            ui,
            t,
            Icon::Headphones,
            32.0,
            if app.view.deafened {
                "Undeafen"
            } else {
                "Deafen"
            },
        );
        if app.view.deafened {
            strike(ui, deaf.rect, t.danger);
        }
        if deaf.clicked() {
            actions.push(Action::ToggleDeafen);
        }
        let mic = icon_button(
            ui,
            t,
            Icon::Mic,
            32.0,
            if app.view.mic_muted { "Unmute" } else { "Mute" },
        );
        if app.view.mic_muted {
            strike(ui, mic.rect, t.danger);
        }
        if mic.clicked() {
            actions.push(Action::ToggleMute);
        }

        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(2.0);
            // One click target for avatar + name + status (plain painting, no selectable
            // labels that would swallow the click). Opens the status picker.
            let size = Vec2::new(ui.available_width() - 2.0, 44.0);
            let sense = if dot.is_some() {
                Sense::click()
            } else {
                Sense::hover()
            };
            let (area, resp) = ui.allocate_exact_size(size, sense);
            if resp.hovered() && dot.is_some() {
                ui.painter().rect_filled(
                    area,
                    CornerRadius::same(4),
                    Color32::from_white_alpha(if t.dark { 10 } else { 40 }),
                );
            }
            let c = Pos2::new(area.left() + 4.0 + 16.0, area.center().y);
            app.media.borrow_mut().paint_avatar(ui, c, 32.0, &me, None);
            let ring = user_card_fill(t);
            match dot {
                Some(s) => super::presence::paint_status(ui, t, c, 32.0, ring, s, false),
                None => paint_status_dot(ui, c, 32.0, ring, t.idle),
            }
            let left = area.left() + 44.0;
            let clip = Rect::from_min_max(Pos2::new(left, area.top()), area.right_bottom())
                .intersect(ui.clip_rect());
            let p = ui.painter().with_clip_rect(clip);
            let name = emoji_text::layout(
                &p,
                me.display_name(),
                bold(14.0),
                t.strong,
                f32::INFINITY,
                0,
            );
            let sub = emoji_text::layout(&p, status, prop(12.0), t.muted, f32::INFINITY, 0);
            let name_h = name.size().y;
            let top = area.center().y - (name_h + sub.size().y) / 2.0;
            name.paint(&p, Pos2::new(left, top), &app.media);
            sub.paint(&p, Pos2::new(left, top + name_h), &app.media);
            if dot.is_some() {
                super::presence::status_menu(&resp, app);
            }
        });
    });
}

fn user_panel_subtitle(custom_status: Option<&str>, status: nixcord_model::Status) -> &str {
    custom_status
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| status.label())
}

fn strike(ui: &Ui, rect: Rect, color: Color32) {
    let r = rect.shrink(8.0);
    ui.painter()
        .line_segment([r.left_top(), r.right_bottom()], Stroke::new(2.0, color));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcord_model::Status;

    #[test]
    fn mention_badge_formatting_and_width_reserve() {
        assert_eq!(badge_text(1), "1");
        assert_eq!(badge_text(99), "99");
        assert_eq!(badge_text(100), "99+");
        assert_eq!(badge_reserve(0), 0.0);
        assert!(badge_reserve(5) > 0.0);
        assert!(badge_reserve(120) > badge_reserve(5));
    }

    #[test]
    fn user_panel_subtitle_prefers_custom_status() {
        assert_eq!(
            user_panel_subtitle(Some("☕ Hacking on Rust"), Status::Online),
            "☕ Hacking on Rust"
        );
        assert_eq!(user_panel_subtitle(Some("   "), Status::Idle), "Idle");
        assert_eq!(user_panel_subtitle(None, Status::Dnd), "Do Not Disturb");
    }

    #[test]
    fn context_menu_items_reflect_read_and_thread_state() {
        let gid = Snowflake(10);
        let g_read = guild_rail_context_items(gid, false, &[]);
        assert_eq!(g_read[0].label, "Mark Server as Read");
        assert!(!g_read[0].enabled);
        assert!(g_read[0].action.is_none());
        let copy_item = g_read.last().unwrap();
        assert_eq!(copy_item.label, "Copy Server ID");
        assert!(copy_item.enabled);
        assert!(matches!(
            copy_item.action,
            Some(Action::Copy(ref s)) if s == "10"
        ));

        let g_unread = guild_rail_context_items(gid, true, &[]);
        assert!(g_unread[0].enabled);
        assert!(matches!(
            g_unread[0].action,
            Some(Action::MarkGuildRead(Snowflake(10)))
        ));

        let cid = Snowflake(102);
        let c_items = channel_context_items(cid, true);
        assert_eq!(c_items[0].label, "Mark as Read");
        assert!(c_items[0].enabled);
        assert!(matches!(
            c_items[0].action,
            Some(Action::MarkChannelRead(Snowflake(102)))
        ));
        assert_eq!(c_items[1].label, "Copy ID");
        assert!(matches!(
            c_items[1].action,
            Some(Action::Copy(ref s)) if s == "102"
        ));

        let tid = Snowflake(111);
        let t_active = thread_context_items(tid, false, false, false);
        assert!(!t_active[0].enabled);
        assert_eq!(t_active[1].label, "Archive Thread");
        assert!(matches!(
            t_active[1].action,
            Some(Action::UpdateThread {
                thread_id: Snowflake(111),
                archived: Some(true),
                locked: None,
            })
        ));
        assert_eq!(t_active[2].label, "Lock Thread");
        assert!(matches!(
            t_active[2].action,
            Some(Action::UpdateThread {
                thread_id: Snowflake(111),
                archived: None,
                locked: Some(true),
            })
        ));

        let t_archived_locked = thread_context_items(tid, true, true, true);
        assert!(t_archived_locked[0].enabled);
        assert_eq!(t_archived_locked[1].label, "Unarchive Thread");
        assert!(matches!(
            t_archived_locked[1].action,
            Some(Action::UpdateThread {
                thread_id: Snowflake(111),
                archived: Some(false),
                locked: None,
            })
        ));
        assert_eq!(t_archived_locked[2].label, "Unlock Thread");
        assert!(matches!(
            t_archived_locked[2].action,
            Some(Action::UpdateThread {
                thread_id: Snowflake(111),
                archived: None,
                locked: Some(false),
            })
        ));
    }

    #[test]
    fn reconcile_and_rearrange_server_folders() {
        let g1 = Snowflake(10);
        let g2 = Snowflake(20);
        let g3 = Snowflake(30);
        let g4 = Snowflake(40);
        let mut layout = Vec::new();
        let mut collapsed = HashSet::new();

        // Initial seed from remote folders + newly joined g4.
        let remote = vec![
            GuildFolder {
                id: Some(500),
                guild_ids: vec![g2, g1],
                name: Some("Rust".into()),
                color: Some(0xE74C3C),
            },
            GuildFolder {
                id: None,
                guild_ids: vec![g3],
                name: None,
                color: None,
            },
        ];
        reconcile_guild_layout(&mut layout, &remote, &[g1, g2, g3, g4]);
        assert_eq!(layout.len(), 3);
        assert!(layout[0].is_folder());
        assert_eq!(layout[0].guild_ids, vec![g2, g1]);
        assert!(!layout[1].is_folder());
        assert_eq!(layout[1].guild_ids, vec![g3]);
        assert_eq!(layout[2].guild_ids, vec![g4]);

        // Reorder within folder: Move g1 up inside folder 500 -> [g1, g2].
        apply_rail_op(
            &mut layout,
            &mut collapsed,
            RailOp::MoveGuild {
                guild_id: g1,
                delta: -1,
            },
        );
        assert_eq!(layout[0].guild_ids, vec![g1, g2]);

        // Drag g4 onto g3 to group them into a new folder.
        apply_rail_op(
            &mut layout,
            &mut collapsed,
            RailOp::Drop {
                source: RailDrag::Guild(g4),
                target: RailDropTarget::GroupWithGuild(g3),
            },
        );
        assert_eq!(layout.len(), 2);
        assert!(layout[1].is_folder());
        assert_eq!(layout[1].guild_ids, vec![g3, g4]);
        let second_fid = layout[1].id.unwrap();
        assert_ne!(second_fid, 500);

        // Move second folder above first folder.
        apply_rail_op(
            &mut layout,
            &mut collapsed,
            RailOp::MoveFolder {
                folder_id: second_fid,
                delta: -1,
            },
        );
        assert_eq!(layout[0].id, Some(second_fid));
        assert_eq!(layout[1].id, Some(500));

        // Remove g4 from the unnamed folder -> unnamed folder unwraps back to standalone g3.
        apply_rail_op(&mut layout, &mut collapsed, RailOp::RemoveFromFolder(g4));
        assert_eq!(layout.len(), 3);
        assert!(!layout[0].is_folder());
        assert_eq!(layout[0].guild_ids, vec![g3]);
        assert!(!layout[1].is_folder());
        assert_eq!(layout[1].guild_ids, vec![g4]);
        assert_eq!(layout[2].id, Some(500));

        // Drag g3 before g4, then after folder 500.
        apply_rail_op(
            &mut layout,
            &mut collapsed,
            RailOp::Drop {
                source: RailDrag::Guild(g3),
                target: RailDropTarget::AfterFolder(500),
            },
        );
        assert_eq!(layout[0].guild_ids, vec![g4]);
        assert_eq!(layout[1].id, Some(500));
        assert_eq!(layout[2].guild_ids, vec![g3]);

        // Ungroup folder 500 -> unpacks [g1, g2] in place.
        apply_rail_op(&mut layout, &mut collapsed, RailOp::UngroupFolder(500));
        let top_ids: Vec<Snowflake> = layout.iter().map(|f| f.guild_ids[0]).collect();
        assert_eq!(top_ids, vec![g4, g1, g2, g3]);
        assert!(layout.iter().all(|f| !f.is_folder()));
    }
}

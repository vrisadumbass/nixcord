//! Positional member list state built from GUILD_MEMBER_LIST_UPDATE ops.
//!
//! Discord models the sidebar as one flat list of rows: group headers ("Admin", "Online",
//! "Offline") and members, addressed by absolute index. SYNC fills a 100-row range,
//! INSERT / DELETE shift the rows after the index, UPDATE replaces one row and INVALIDATE
//! forgets a range (we were unsubscribed from it). Rows only hold user ids; member data
//! and presences live in the store's caches. Rows past [`MEMBER_LIST_MAX_ROWS`] are never
//! kept, which bounds memory no matter how large the guild is.

use nixcord_model::{MemberListGroup, MemberListItem, MemberListOp, MemberListUpdate, Snowflake};

/// We subscribe to at most three 100-row ranges per guild.
pub const MEMBER_LIST_MAX_ROWS: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub enum ListRow {
    Group(MemberListGroup),
    Member(Snowflake),
}

/// A group header plus the members under it that are currently loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct ListSection {
    pub group: MemberListGroup,
    pub members: Vec<Snowflake>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberList {
    /// "everyone" or the overwrite hash Discord uses to share lists between channels.
    pub id: String,
    pub member_count: u32,
    pub online_count: u32,
    /// All groups with their full counts (also for rows we don't hold).
    pub groups: Vec<MemberListGroup>,
    /// Absolute row index -> row. `None`: not synced (or undecodable).
    rows: Vec<Option<ListRow>>,
}

/// Every item carried by a batch of ops (for caching member data / presences).
pub(crate) fn op_items(ops: &[MemberListOp]) -> impl Iterator<Item = &MemberListItem> {
    ops.iter()
        .flat_map(|op| -> Box<dyn Iterator<Item = &MemberListItem>> {
            match op {
                MemberListOp::Sync { items, .. } => Box::new(items.iter()),
                MemberListOp::Insert { item, .. } | MemberListOp::Update { item, .. } => {
                    Box::new(std::iter::once(item))
                }
                _ => Box::new(std::iter::empty()),
            }
        })
}

fn to_row(item: &MemberListItem) -> Option<ListRow> {
    match item {
        MemberListItem::Group(g) => Some(ListRow::Group(g.clone())),
        MemberListItem::Member(m) => m.member.id().map(ListRow::Member),
        MemberListItem::Unknown => None,
    }
}

impl MemberList {
    pub fn new(id: String) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    pub fn rows(&self) -> &[Option<ListRow>] {
        &self.rows
    }

    /// Rows the full list has on Discord's side: every non-empty group's header plus
    /// its members.
    pub fn total_rows(&self) -> u32 {
        self.groups
            .iter()
            .filter(|g| g.count > 0)
            .map(|g| g.count.saturating_add(1))
            .fold(0u32, u32::saturating_add)
    }

    pub fn member_ids(&self) -> impl Iterator<Item = Snowflake> + '_ {
        self.rows.iter().filter_map(|r| match r {
            Some(ListRow::Member(id)) => Some(*id),
            _ => None,
        })
    }

    /// Loaded rows grouped under their headers, in list order. Counts come from the
    /// top-level `groups` (full counts), falling back to the header item's own count.
    /// Members before any header land in a section with an empty group id.
    pub fn sections(&self) -> Vec<ListSection> {
        let mut out: Vec<ListSection> = Vec::new();
        for row in self.rows.iter().flatten() {
            match row {
                ListRow::Group(g) => {
                    let count = self
                        .groups
                        .iter()
                        .find(|x| x.id == g.id)
                        .map_or(g.count, |x| x.count);
                    out.push(ListSection {
                        group: MemberListGroup {
                            id: g.id.clone(),
                            count,
                        },
                        members: Vec::new(),
                    });
                }
                ListRow::Member(id) => {
                    if out.is_empty() {
                        out.push(ListSection {
                            group: MemberListGroup::default(),
                            members: Vec::new(),
                        });
                    }
                    if let Some(s) = out.last_mut() {
                        s.members.push(*id);
                    }
                }
            }
        }
        out
    }

    pub(crate) fn apply(&mut self, u: &MemberListUpdate) {
        self.member_count = u.member_count;
        self.online_count = u.online_count;
        if !u.groups.is_empty() {
            self.groups.clone_from(&u.groups);
        }
        for op in &u.ops {
            self.apply_op(op);
        }
        self.trim();
    }

    fn ensure_len(&mut self, len: usize) {
        let len = len.min(MEMBER_LIST_MAX_ROWS);
        if self.rows.len() < len {
            self.rows.resize(len, None);
        }
    }

    fn apply_op(&mut self, op: &MemberListOp) {
        const MAX: usize = MEMBER_LIST_MAX_ROWS;
        match op {
            MemberListOp::Sync { range, items } => {
                let start = range.0 as usize;
                let end = range.1 as usize;
                if start >= MAX {
                    return;
                }
                self.ensure_len(start + items.len());
                for (i, item) in items.iter().enumerate() {
                    let idx = start + i;
                    if idx >= MAX {
                        break;
                    }
                    self.rows[idx] = to_row(item);
                }
                // The list is shorter than the range: forget stale rows in the tail.
                let filled = start + items.len();
                if filled <= end {
                    for r in self.rows.iter_mut().take(end + 1).skip(filled) {
                        *r = None;
                    }
                }
            }
            MemberListOp::Insert { index, item } => {
                let idx = *index as usize;
                if idx >= MAX {
                    return;
                }
                self.ensure_len(idx);
                self.rows.insert(idx, to_row(item));
                self.rows.truncate(MAX);
            }
            MemberListOp::Update { index, item } => {
                let idx = *index as usize;
                if idx >= MAX {
                    return;
                }
                self.ensure_len(idx + 1);
                self.rows[idx] = to_row(item);
            }
            MemberListOp::Delete { index } => {
                let idx = *index as usize;
                if idx < self.rows.len() {
                    self.rows.remove(idx);
                }
            }
            MemberListOp::Invalidate { range } => {
                let start = range.0 as usize;
                let end = range.1 as usize;
                for r in self.rows.iter_mut().take(end.saturating_add(1)).skip(start) {
                    *r = None;
                }
            }
        }
    }

    fn trim(&mut self) {
        while matches!(self.rows.last(), Some(None)) {
            self.rows.pop();
        }
    }
}

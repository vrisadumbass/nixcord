//! `@` / `#` autocomplete for the composer, kept free of egui: finding the query before
//! the caret, ranking candidates, and turning the readable text in the box into
//! Discord's wire form when the message is sent.
//!
//! Wire forms (docs.discord.com/developers, "Message Formatting" reference): `<@USER_ID>`,
//! `<@&ROLE_ID>` and `<#CHANNEL_ID>`; `@everyone` and `@here` are plain text.
//!
//! The box shows `@name`. Each pick is remembered in [`MentionSpans`] as (byte start,
//! display text, wire text). The composer calls [`MentionSpans::sync`] with the current
//! text every frame; it diffs that against the text it saw last (common prefix and
//! suffix, which is exact for the single edit a frame makes: typing, a paste, deleting a
//! selection, an inserted emoji), moves the spans after the edit, and drops the spans the
//! edit touched. A span also stops counting when its text no longer reads `display` or
//! runs straight into a word character (`@ada` typed on into `@adam`). On send,
//! [`MentionSpans::to_wire`] swaps the surviving spans for their wire text; anything else
//! is sent as typed. Sending or switching channels clears the spans.

use nixcord_model::{ChannelKind, Snowflake};

/// The popup shows at most this many rows.
pub const MAX_ROWS: usize = 10;
/// A longer "query" is ordinary text, not a mention being typed.
const MAX_QUERY_CHARS: usize = 32;

/// What the typed character asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// `@`: users, roles, `@everyone` / `@here`.
    Mention,
    /// `#`: channels of the server.
    Channel,
}

/// An open `@query` / `#query`: the trigger sits at byte `start`, `text` follows it up
/// to the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query<'a> {
    pub start: usize,
    pub trigger: Trigger,
    pub text: &'a str,
}

/// The `@` / `#` query that ends at the caret (`before` = text before the caret), if the
/// trigger starts a word: at the start, after whitespace or after opening markdown
/// (`(`, `*`, `_`, `~`, `|`, quotes). `mail@host` and `a#b` never open the popup.
pub fn mention_query(before: &str) -> Option<Query<'_>> {
    let (start, c) = before
        .char_indices()
        .rev()
        .find(|&(_, c)| c == '@' || c == '#' || ends_query(c))?;
    let trigger = match c {
        '@' => Trigger::Mention,
        '#' => Trigger::Channel,
        _ => return None,
    };
    let prev = before[..start].chars().next_back();
    if !prev.is_none_or(|p| p.is_whitespace() || "([{\"'*_~|".contains(p)) {
        return None;
    }
    let text = &before[start + 1..];
    (text.chars().count() <= MAX_QUERY_CHARS).then_some(Query {
        start,
        trigger,
        text,
    })
}

/// Characters no username, role or channel query contains.
fn ends_query(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | ':' | '`')
}

/// A user `@` can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserCand<'a> {
    pub id: Snowflake,
    pub username: &'a str,
    pub global_name: Option<&'a str>,
    /// Server nickname.
    pub nick: Option<&'a str>,
    /// Rank among the channel's recent authors (0 = newest).
    pub recent: Option<u32>,
}

impl UserCand<'_> {
    /// Nickname, else display name, else username: what the chat shows.
    pub fn shown(&self) -> &str {
        self.nick
            .or(self.global_name)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(self.username)
    }
}

/// A role `@` can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleCand<'a> {
    pub id: Snowflake,
    pub name: &'a str,
    /// 0xRRGGBB, 0 = no colour.
    pub color: u32,
}

/// A channel `#` can offer, in sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCand<'a> {
    pub id: Snowflake,
    pub name: &'a str,
    pub kind: ChannelKind,
}

/// One popup row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionItem {
    User {
        id: Snowflake,
        /// Nickname / display name, as the chat shows it.
        name: String,
        username: String,
    },
    Role {
        id: Snowflake,
        name: String,
        color: u32,
    },
    Everyone,
    Here,
    Channel {
        id: Snowflake,
        name: String,
        kind: ChannelKind,
    },
}

impl MentionItem {
    /// What goes into the text box. Users by username: unique and without spaces, like
    /// the official client.
    pub fn display(&self) -> String {
        match self {
            Self::User { username, .. } => format!("@{username}"),
            Self::Role { name, .. } => format!("@{name}"),
            Self::Everyone => "@everyone".into(),
            Self::Here => "@here".into(),
            Self::Channel { name, .. } => format!("#{name}"),
        }
    }

    /// What is sent.
    pub fn wire(&self) -> String {
        match self {
            Self::User { id, .. } => format!("<@{id}>"),
            Self::Role { id, .. } => format!("<@&{id}>"),
            Self::Everyone | Self::Here => self.display(),
            Self::Channel { id, .. } => format!("<#{id}>"),
        }
    }
}

/// 0: some name starts with the query, 1: some name contains it, `None`: no match.
/// Case-insensitive; `query` is already lowercase. An empty query matches everything.
fn match_tier(query: &str, names: &[&str]) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let mut best = None;
    for n in names {
        let n = n.to_lowercase();
        if n.starts_with(query) {
            return Some(0);
        }
        if n.contains(query) {
            best = Some(1);
        }
    }
    best
}

/// Rank `@` candidates for `query`: prefix matches (nickname, display name or username)
/// before substring matches; then users before roles before `@everyone` / `@here`; then
/// the channel's recent authors, newest first; then by name. At most `cap` rows.
/// `everyone`: may I use `@everyone` / `@here` here.
pub fn rank_mentions(
    query: &str,
    users: &[UserCand<'_>],
    roles: &[RoleCand<'_>],
    everyone: bool,
    cap: usize,
) -> Vec<MentionItem> {
    let q = query.to_lowercase();
    // (tier, kind, recent rank, lowercase name, item)
    let mut ranked: Vec<(u8, u8, u32, String, MentionItem)> = Vec::new();
    for u in users {
        let names = [
            u.nick.unwrap_or(""),
            u.global_name.unwrap_or(""),
            u.username,
        ];
        if let Some(tier) = match_tier(&q, &names) {
            ranked.push((
                tier,
                0,
                u.recent.unwrap_or(u32::MAX),
                u.shown().to_lowercase(),
                MentionItem::User {
                    id: u.id,
                    name: u.shown().to_string(),
                    username: u.username.to_string(),
                },
            ));
        }
    }
    for r in roles {
        if let Some(tier) = match_tier(&q, &[r.name]) {
            ranked.push((
                tier,
                1,
                u32::MAX,
                r.name.to_lowercase(),
                MentionItem::Role {
                    id: r.id,
                    name: r.name.to_string(),
                    color: r.color,
                },
            ));
        }
    }
    if everyone {
        for (name, item) in [
            ("everyone", MentionItem::Everyone),
            ("here", MentionItem::Here),
        ] {
            if let Some(tier) = match_tier(&q, &[name]) {
                ranked.push((tier, 2, u32::MAX, name.to_string(), item));
            }
        }
    }
    ranked.sort_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
    ranked.truncate(cap);
    ranked.into_iter().map(|r| r.4).collect()
}

/// Rank `#` candidates: prefix matches before substring matches, then sidebar order.
pub fn rank_channels(query: &str, channels: &[ChannelCand<'_>], cap: usize) -> Vec<MentionItem> {
    let q = query.to_lowercase();
    let mut ranked: Vec<(u8, usize, MentionItem)> = channels
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let tier = match_tier(&q, &[c.name])?;
            Some((
                tier,
                i,
                MentionItem::Channel {
                    id: c.id,
                    name: c.name.to_string(),
                    kind: c.kind,
                },
            ))
        })
        .collect();
    ranked.sort_by_key(|r| (r.0, r.1));
    ranked.truncate(cap);
    ranked.into_iter().map(|r| r.2).collect()
}

/// One inserted mention: `display` at byte `start` of the composer text stands for `wire`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    start: usize,
    display: String,
    wire: String,
}

impl Span {
    fn end(&self) -> usize {
        self.start + self.display.len()
    }

    /// Does `text` still read `display` here, not run on into a longer word?
    fn holds(&self, text: &str) -> bool {
        text.get(self.start..self.end()) == Some(self.display.as_str())
            && !text[self.end()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }
}

/// Mentions picked from the popup, tracked through edits (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct MentionSpans {
    /// Sorted by `start`, non-overlapping.
    spans: Vec<Span>,
    /// The text the spans refer to. Empty while there are no spans.
    snapshot: String,
}

impl MentionSpans {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.snapshot.clear();
    }

    /// Follow the edit from the last seen text to `text`.
    pub fn sync(&mut self, text: &str) {
        if self.spans.is_empty() {
            self.snapshot.clear();
            return;
        }
        if self.snapshot == text {
            return;
        }
        let (prefix, old_end, new_end) = edit_range(&self.snapshot, text);
        let delta = new_end as isize - old_end as isize;
        self.spans.retain_mut(|s| {
            if s.end() <= prefix {
                true
            } else if s.start >= old_end {
                s.start = s.start.wrapping_add_signed(delta);
                true
            } else {
                false
            }
        });
        self.spans.retain(|s| s.holds(text));
        self.snapshot.clear();
        if !self.spans.is_empty() {
            self.snapshot.push_str(text);
        }
    }

    /// Remember that `display` was just inserted at byte `start` of `text` (which already
    /// contains it) and stands for `wire`. Plain-text picks (`@everyone`) need no span.
    pub fn add(&mut self, text: &str, start: usize, display: &str, wire: &str) {
        self.sync(text);
        if display == wire {
            return;
        }
        let span = Span {
            start,
            display: display.to_string(),
            wire: wire.to_string(),
        };
        if !span.holds(text) {
            return;
        }
        self.spans
            .retain(|s| s.end() <= span.start || s.start >= span.end());
        let at = self.spans.partition_point(|s| s.start < span.start);
        self.spans.insert(at, span);
        self.snapshot.clear();
        self.snapshot.push_str(text);
    }

    /// `text` with every surviving span replaced by its wire form.
    pub fn to_wire(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for s in &self.spans {
            if s.start < copied || !s.holds(text) {
                continue;
            }
            out.push_str(&text[copied..s.start]);
            out.push_str(&s.wire);
            copied = s.end();
        }
        out.push_str(&text[copied..]);
        out
    }
}

/// The single edit that turns `old` into `new`: bytes `prefix..old_end` of `old` became
/// `prefix..new_end` of `new` (longest common prefix, then longest common suffix that
/// doesn't overlap it). All three are char boundaries.
fn edit_range(old: &str, new: &str) -> (usize, usize, usize) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let mut prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    // The bytes before `prefix` are equal, so a char that starts before it starts in both
    // strings with the same lead byte: backing up to a boundary of `old` fits `new` too.
    while !old.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let room = a.len().min(b.len()) - prefix;
    let mut suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(room)
        .take_while(|(x, y)| x == y)
        .count();
    // Equal trailing bytes: a boundary at `len - suffix` in one is one in the other.
    while !old.is_char_boundary(a.len() - suffix) {
        suffix -= 1;
    }
    (prefix, a.len() - suffix, b.len() - suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user<'a>(id: u64, username: &'a str, nick: Option<&'a str>) -> UserCand<'a> {
        UserCand {
            id: Snowflake(id),
            username,
            global_name: None,
            nick,
            recent: None,
        }
    }

    fn names(items: &[MentionItem]) -> Vec<String> {
        items.iter().map(MentionItem::display).collect()
    }

    #[test]
    fn query_opens_only_at_a_word_start() {
        let q = mention_query("hi @ad").unwrap();
        assert_eq!((q.start, q.trigger, q.text), (3, Trigger::Mention, "ad"));
        let q = mention_query("@").unwrap();
        assert_eq!((q.start, q.text), (0, ""));
        let q = mention_query("see #gen").unwrap();
        assert_eq!((q.start, q.trigger, q.text), (4, Trigger::Channel, "gen"));
        assert_eq!(mention_query("(@x").unwrap().text, "x");
        assert_eq!(mention_query("**@x").unwrap().text, "x");
        assert!(mention_query("mail@host").is_none());
        assert!(mention_query("a#b").is_none());
        assert!(mention_query("@ada done").is_none(), "a space ends it");
        assert!(mention_query("<@123").is_none());
        assert!(mention_query("`@x").is_none());
        assert!(mention_query("no trigger").is_none());
        assert!(mention_query(&format!("@{}", "a".repeat(33))).is_none());
        // Multi-byte text before the trigger.
        let q = mention_query("héllo ünï @mä").unwrap();
        assert_eq!(q.text, "mä");
    }

    #[test]
    fn prefix_matches_come_before_substring_matches() {
        let users = [user(1, "xadamx", None), user(2, "adam", None)];
        let got = rank_mentions("ad", &users, &[], false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@adam", "@xadamx"]);
        assert!(rank_mentions("zz", &users, &[], false, MAX_ROWS).is_empty());
    }

    #[test]
    fn nickname_display_name_and_username_all_match() {
        let users = [
            user(1, "quill", Some("Captain")),
            UserCand {
                global_name: Some("Sky Rider"),
                ..user(2, "rider42", None)
            },
            user(3, "capybara", None),
        ];
        // Nickname prefix and username prefix tie; sorted by shown name.
        let got = rank_mentions("cap", &users, &[], false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@quill", "@capybara"]);
        match &got[0] {
            MentionItem::User { name, .. } => assert_eq!(name, "Captain"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            names(&rank_mentions("sky", &users, &[], false, MAX_ROWS)),
            vec!["@rider42"]
        );
        // Case-insensitive, and the username still matches when a nickname is set.
        assert_eq!(
            names(&rank_mentions("QUI", &users, &[], false, MAX_ROWS)),
            vec!["@quill"]
        );
        // Only a substring of both "rider42" and "Sky Rider": still offered.
        assert_eq!(
            names(&rank_mentions("ider", &users, &[], false, MAX_ROWS)),
            vec!["@rider42"]
        );
    }

    #[test]
    fn recent_authors_first_among_equals() {
        let users = [
            user(1, "amy", None),
            UserCand {
                recent: Some(1),
                ..user(2, "abe", None)
            },
            UserCand {
                recent: Some(0),
                ..user(3, "ava", None)
            },
            user(4, "bea", None),
        ];
        let got = rank_mentions("a", &users, &[], false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@ava", "@abe", "@amy", "@bea"]);
        // A recent author never beats a better match.
        let got = rank_mentions("be", &users, &[], false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@bea", "@abe"]);
    }

    #[test]
    fn list_is_capped() {
        let owned: Vec<String> = (0..30).map(|i| format!("user{i:02}")).collect();
        let users: Vec<UserCand<'_>> = owned
            .iter()
            .enumerate()
            .map(|(i, n)| user(i as u64, n, None))
            .collect();
        let got = rank_mentions("", &users, &[], true, MAX_ROWS);
        assert_eq!(got.len(), MAX_ROWS);
        assert_eq!(got[0].display(), "@user00");
    }

    #[test]
    fn roles_and_everyone() {
        let users = [user(1, "modest", None)];
        let roles = [
            RoleCand {
                id: Snowflake(70),
                name: "mod",
                color: 0xE74C3C,
            },
            RoleCand {
                id: Snowflake(71),
                name: "artist",
                color: 0,
            },
        ];
        let got = rank_mentions("mod", &users, &roles, true, MAX_ROWS);
        assert_eq!(names(&got), vec!["@modest", "@mod"]);
        assert_eq!(got[1].wire(), "<@&70>");
        let got = rank_mentions("e", &users, &roles, true, MAX_ROWS);
        assert_eq!(names(&got), vec!["@everyone", "@modest", "@here"]);
        assert_eq!(got[0].wire(), "@everyone");
        assert_eq!(MentionItem::Here.wire(), "@here");
        // Without the permission they are not offered.
        let got = rank_mentions("e", &users, &roles, false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@modest"]);
    }

    #[test]
    fn dm_recipients_rank_like_members() {
        // A DM offers its recipients only: no roles, no @everyone.
        let users = [user(4, "cy", None), user(5, "dee", None)];
        let got = rank_mentions("", &users, &[], false, MAX_ROWS);
        assert_eq!(names(&got), vec!["@cy", "@dee"]);
        assert_eq!(got[0].wire(), "<@4>");
    }

    #[test]
    fn channels_rank_prefix_then_sidebar_order() {
        let chans = [
            ChannelCand {
                id: Snowflake(1),
                name: "off-topic",
                kind: ChannelKind::Text,
            },
            ChannelCand {
                id: Snowflake(2),
                name: "general",
                kind: ChannelKind::Text,
            },
            ChannelCand {
                id: Snowflake(3),
                name: "topic-ideas",
                kind: ChannelKind::Forum,
            },
        ];
        let got = rank_channels("topic", &chans, MAX_ROWS);
        assert_eq!(names(&got), vec!["#topic-ideas", "#off-topic"]);
        assert_eq!(got[0].wire(), "<#3>");
        assert_eq!(rank_channels("", &chans, 2).len(), 2);
    }

    /// Simulates the composer: text edits followed by `sync`.
    fn edit(spans: &mut MentionSpans, text: &mut String, at: usize, remove: usize, insert: &str) {
        text.replace_range(at..at + remove, insert);
        spans.sync(text);
    }

    #[test]
    fn wire_conversion_survives_edits_elsewhere() {
        let mut text = String::from("hi @ada ");
        let mut spans = MentionSpans::default();
        spans.add(&text, 3, "@ada", "<@2>");
        assert_eq!(spans.to_wire(&text), "hi <@2> ");
        // Typing after, and before, the span.
        let end = text.len();
        edit(&mut spans, &mut text, end, 0, "and #general ");
        spans.add(&text, 12, "#general", "<#9>");
        edit(&mut spans, &mut text, 0, 2, "hello there");
        assert_eq!(text, "hello there @ada and #general ");
        assert_eq!(spans.to_wire(&text), "hello there <@2> and <#9> ");
        // Multi-byte edits before the spans.
        edit(&mut spans, &mut text, 0, 0, "ünï ");
        assert_eq!(spans.to_wire(&text), "ünï hello there <@2> and <#9> ");
    }

    #[test]
    fn edits_that_break_a_span_drop_it() {
        let mut text = String::from("@ada @bo ");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@ada", "<@2>");
        spans.add(&text, 5, "@bo", "<@3>");
        // Delete a letter inside the first: it is sent as typed.
        edit(&mut spans, &mut text, 2, 1, "");
        assert_eq!(text, "@aa @bo ");
        assert_eq!(spans.to_wire(&text), "@aa <@3> ");
        // Typing straight on after a mention breaks it too.
        let mut text = String::from("@ada");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@ada", "<@2>");
        edit(&mut spans, &mut text, 4, 0, "m");
        assert_eq!(spans.to_wire(&text), "@adam");
        assert!(spans.is_empty());
        // Retyping the same text by hand does not bring it back.
        edit(&mut spans, &mut text, 4, 1, "");
        assert_eq!(spans.to_wire(&text), "@ada");
    }

    #[test]
    fn select_all_and_replace_clears_spans() {
        let mut text = String::from("@ada ");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@ada", "<@2>");
        text.clear();
        text.push_str("new text");
        spans.sync(&text);
        assert!(spans.is_empty());
        assert_eq!(spans.to_wire(&text), "new text");
    }

    #[test]
    fn repeated_characters_at_a_span_edge() {
        // "@ann" then another "n" typed at its end: the diff can't tell where the n went,
        // but the span runs into a word character either way and is dropped.
        let mut text = String::from("@ann");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@ann", "<@5>");
        edit(&mut spans, &mut text, 2, 0, "n");
        assert_eq!(spans.to_wire(&text), "@annn");
        // Space typed after a mention keeps it.
        let mut text = String::from("x@ann");
        text.replace_range(0..1, "");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@ann", "<@5>");
        edit(&mut spans, &mut text, 4, 0, " ");
        assert_eq!(spans.to_wire(&text), "<@5> ");
    }

    #[test]
    fn same_name_twice_keeps_both_ids() {
        // Two roles with the same name: positions keep them apart.
        let mut text = String::from("@team @team ");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@team", "<@&1>");
        spans.add(&text, 6, "@team", "<@&2>");
        assert_eq!(spans.to_wire(&text), "<@&1> <@&2> ");
        edit(&mut spans, &mut text, 6, 0, "and ");
        assert_eq!(spans.to_wire(&text), "<@&1> and <@&2> ");
        // Deleting one of two identical runs is ambiguous to a text diff (it reads as
        // the later one going); one mention survives either way.
        edit(&mut spans, &mut text, 0, 10, "");
        assert_eq!(text, "@team ");
        let wire = spans.to_wire(&text);
        assert!(wire == "<@&1> " || wire == "<@&2> ", "{wire}");
    }

    #[test]
    fn plain_picks_and_stale_adds_need_no_span() {
        let text = String::from("@everyone ");
        let mut spans = MentionSpans::default();
        spans.add(&text, 0, "@everyone", "@everyone");
        assert!(spans.is_empty());
        // A span that doesn't match the text is ignored.
        spans.add(&text, 0, "@ada", "<@2>");
        assert!(spans.is_empty());
        assert_eq!(spans.to_wire(&text), "@everyone ");
    }

    #[test]
    fn edit_range_is_on_char_boundaries() {
        assert_eq!(edit_range("abc", "abXc"), (2, 2, 3));
        assert_eq!(edit_range("é", "è"), (0, 2, 2));
        assert_eq!(edit_range("aaa", "aa"), (2, 3, 2));
        assert_eq!(edit_range("", "x"), (0, 0, 1));
        assert_eq!(edit_range("same", "same"), (4, 4, 4));
    }
}

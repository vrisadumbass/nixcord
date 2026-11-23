//! Finding emoji in text, and shortcode handling for the composer.

use crate::{by_shortcode, find, is_tone, VS16};
use std::borrow::Cow;
use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};

/// A piece of text: plain text, or one emoji grapheme cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOrEmoji<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

/// Is this grapheme cluster a unicode emoji we have data (and a Twemoji image) for?
///
/// Uses the emoji table, never character ranges, so digits, `#`, `*` and other text that
/// merely *can* start an emoji stay text. Characters that Unicode shows as text by default
/// (©, ™, ❤ without U+FE0F, ...) count only with U+FE0F or a skin-tone modifier.
pub fn is_emoji(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // Every emoji starts at U+00A9 or above, except keycaps (`1️⃣`), which need U+20E3.
    if (first as u32) < 0xA9 && !cluster.contains('\u{20E3}') {
        return false;
    }
    let Some(e) = find(cluster) else {
        return false;
    };
    if e.text_by_default() {
        return cluster.chars().any(|c| c == VS16 || is_tone(c));
    }
    true
}

/// Iterator returned by [`emoji_runs`].
pub struct EmojiRuns<'a> {
    text: &'a str,
    graphemes: Option<GraphemeIndices<'a>>,
    /// Start of the text run being collected.
    run_start: usize,
    /// An emoji found right after a text run, returned next.
    pending: Option<(usize, &'a str)>,
    done: bool,
}

impl<'a> Iterator for EmojiRuns<'a> {
    type Item = TextOrEmoji<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((end, e)) = self.pending.take() {
            self.run_start = end;
            return Some(TextOrEmoji::Emoji(e));
        }
        if self.done {
            return None;
        }
        let Some(g) = self.graphemes.as_mut() else {
            // ASCII fast path: no emoji possible.
            self.done = true;
            return (!self.text.is_empty()).then_some(TextOrEmoji::Text(self.text));
        };
        for (i, cluster) in g.by_ref() {
            if !is_emoji(cluster) {
                continue;
            }
            let end = i + cluster.len();
            if i > self.run_start {
                let text = &self.text[self.run_start..i];
                self.pending = Some((end, cluster));
                return Some(TextOrEmoji::Text(text));
            }
            self.run_start = end;
            return Some(TextOrEmoji::Emoji(cluster));
        }
        self.done = true;
        (self.run_start < self.text.len()).then(|| TextOrEmoji::Text(&self.text[self.run_start..]))
    }
}

/// Split `text` into plain-text runs and emoji grapheme clusters, in order. Adjacent text
/// clusters are merged; empty text yields nothing. Pure-ASCII text costs one scan.
pub fn emoji_runs(text: &str) -> EmojiRuns<'_> {
    EmojiRuns {
        text,
        graphemes: (!text.is_ascii()).then(|| text.grapheme_indices(true)),
        run_start: 0,
        pending: None,
        done: false,
    }
}

/// `Some(n)` if `text` is only emoji (n of them) and whitespace, `None` otherwise.
pub fn count_emoji_only(text: &str) -> Option<usize> {
    let mut n = 0;
    for run in emoji_runs(text) {
        match run {
            TextOrEmoji::Emoji(_) => n += 1,
            TextOrEmoji::Text(t) if t.trim().is_empty() => {}
            TextOrEmoji::Text(_) => return None,
        }
    }
    Some(n)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+'
}

/// The `:query` being typed at the end of `before_cursor` (text up to the caret), for
/// emoji autocomplete: `Some((byte index of the ':', query))` when the query has at least
/// two name characters and the colon starts a word (`10:30` and `<:custom:` don't count).
pub fn shortcode_query(before_cursor: &str) -> Option<(usize, &str)> {
    let colon = before_cursor.rfind(':')?;
    let query = &before_cursor[colon + 1..];
    if query.chars().count() < 2 || !query.chars().all(is_name_char) {
        return None;
    }
    let prev = before_cursor[..colon].chars().next_back();
    if prev.is_some_and(|p| p.is_alphanumeric() || p == '<' || p == ':' || p == '_') {
        return None;
    }
    Some((colon, query))
}

/// Replace complete `:shortcode:`s with unicode emoji (`:heart:` -> ❤️,
/// `:thumbsup_tone2:` -> 👍🏼). Names that aren't unicode emoji go to `custom` (return the
/// replacement, e.g. `<:name:id>`, or `None` to keep the text). Code spans and blocks and
/// `<:name:id>` custom emoji are left alone.
pub fn replace_shortcodes<'a>(
    text: &'a str,
    mut custom: impl FnMut(&str) -> Option<String>,
) -> Cow<'a, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0; // bytes of `text` already in `out`
    let mut i = 0;
    let mut code: Option<usize> = None; // backtick run length of the open code span
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'`' {
            let run = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            code = match code {
                None => Some(run),
                Some(n) if n == run => None,
                open => open,
            };
            i += run;
            continue;
        }
        if b == b':' && code.is_none() {
            let custom_tag = (i >= 1 && bytes[i - 1] == b'<')
                || (i >= 2 && bytes[i - 1] == b'a' && bytes[i - 2] == b'<');
            let name_len = text[i + 1..]
                .bytes()
                .take_while(|&c| is_name_char(c as char))
                .count();
            let close = i + 1 + name_len;
            if !custom_tag && name_len > 0 && name_len <= 64 && bytes.get(close) == Some(&b':') {
                let name = &text[i + 1..close];
                let rep = match by_shortcode(name) {
                    Some((e, tone)) => Some(e.with_skin_tone(tone).into_owned()),
                    None => custom(name),
                };
                if let Some(rep) = rep {
                    out.push_str(&text[copied..i]);
                    out.push_str(&rep);
                    i = close + 1;
                    copied = i;
                    continue;
                }
                // Not a name: the closing colon may open the next one.
                i = close;
                continue;
            }
        }
        i += 1;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

//! Inline content.
//!
//! Two passes:
//! 1. `tokenize`: one left-to-right scan turns the text into tokens. Opaque things (code
//!    spans, `<...>` tokens, URLs, masked links, escapes) become finished atoms, so
//!    emphasis markers inside them can never match. Every lookahead is bounded.
//! 2. `parse_range`: Discord's (simple-markdown) emphasis rules over the tokens: the first
//!    valid closer wins, inner content is parsed recursively. Closer scans spend a shared
//!    work budget; once it is spent, remaining markers stay literal (linear worst case).

use crate::{Budget, Inline, Link, Mention, TimestampStyle};

const MAX_DEPTH: u8 = 24;
/// Longest `<...>` token we look for (long `<https://...>` URLs included).
const MAX_ANGLE: usize = 2048;
const MAX_LABEL: usize = 1024;

pub(crate) fn parse_inline(src: &str, depth: u8, budget: &mut Budget) -> Vec<Inline> {
    let mut p = InlineParser {
        src,
        toks: Vec::new(),
        budget,
        depth,
    };
    p.tokenize();
    let n = p.toks.len();
    p.parse_range(0, n, depth)
}

#[derive(Debug)]
enum Kind {
    /// Source text `[start, end)`.
    Text,
    /// One of `* _ ~ |`.
    Delim(u8),
    /// A finished node (taken exactly once while building the tree).
    Atom(Option<Inline>),
}

#[derive(Debug)]
struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

struct InlineParser<'a, 'b> {
    src: &'a str,
    toks: Vec<Tok>,
    budget: &'b mut Budget,
    depth: u8,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn push_text(out: &mut Vec<Inline>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Inline::Text(t)) = out.last_mut() {
        t.push_str(s);
    } else {
        out.push(Inline::Text(s.to_string()));
    }
}

fn parse_id(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl InlineParser<'_, '_> {
    // ---- pass 1 ----------------------------------------------------------------------

    fn tokenize(&mut self) {
        let src = self.src;
        let b = src.as_bytes();
        let mut i = 0;
        let mut text_start = 0;
        while i < b.len() {
            let c = b[i];
            let atom: Option<(Inline, usize)> = match c {
                b'\\' => {
                    if let Some(ch) = src[i + 1..].chars().next() {
                        if !ch.is_ascii_alphanumeric() && !ch.is_whitespace() {
                            self.text(text_start, i);
                            let e = i + 1 + ch.len_utf8();
                            self.toks.push(Tok {
                                kind: Kind::Text,
                                start: i + 1,
                                end: e,
                            });
                            i = e;
                            text_start = i;
                            continue;
                        }
                    }
                    None
                }
                b'`' => {
                    let run = b[i..].iter().take_while(|&&x| x == b'`').count();
                    match self.code_span(i, run) {
                        Some(a) => Some(a),
                        None => {
                            i += run;
                            continue;
                        }
                    }
                }
                b'<' => self.angle(i),
                b'h' => {
                    let prev_ok = src[..i]
                        .chars()
                        .next_back()
                        .is_none_or(|p| !p.is_alphanumeric());
                    if prev_ok {
                        self.url(i).map(|(url, e)| {
                            (
                                Inline::Link(Link {
                                    url,
                                    label: None,
                                    embed: true,
                                }),
                                e,
                            )
                        })
                    } else {
                        None
                    }
                }
                b'@' => {
                    let rest = &src[i..];
                    if rest.starts_with("@everyone") {
                        Some((Inline::Mention(Mention::Everyone), i + 9))
                    } else if rest.starts_with("@here") {
                        Some((Inline::Mention(Mention::Here), i + 5))
                    } else {
                        None
                    }
                }
                b'[' => self.masked_link(i),
                b'*' | b'_' | b'~' | b'|' => {
                    self.text(text_start, i);
                    self.toks.push(Tok {
                        kind: Kind::Delim(c),
                        start: i,
                        end: i + 1,
                    });
                    i += 1;
                    text_start = i;
                    continue;
                }
                _ => None,
            };
            match atom {
                Some((node, end)) => {
                    self.text(text_start, i);
                    self.toks.push(Tok {
                        kind: Kind::Atom(Some(node)),
                        start: i,
                        end,
                    });
                    i = end;
                    text_start = i;
                }
                None => i += 1,
            }
        }
        self.text(text_start, b.len());
    }

    fn text(&mut self, start: usize, end: usize) {
        if start < end {
            self.toks.push(Tok {
                kind: Kind::Text,
                start,
                end,
            });
        }
    }

    /// `` `code` `` / ``` ``co`de`` ```: the closer is the next backtick run of the same
    /// length.
    fn code_span(&mut self, i: usize, run: usize) -> Option<(Inline, usize)> {
        let src = self.src;
        let b = src.as_bytes();
        let mut j = i + run;
        loop {
            if !self.budget.tick() {
                return None;
            }
            let k = j + src[j..].find('`')?;
            let r = b[k..].iter().take_while(|&&x| x == b'`').count();
            if r == run {
                let mut code = &src[i + run..k];
                if code.len() >= 2
                    && code.starts_with(' ')
                    && code.ends_with(' ')
                    && !code.trim().is_empty()
                {
                    code = &code[1..code.len() - 1];
                }
                return Some((Inline::Code(code.to_string()), k + r));
            }
            j = k + r;
        }
    }

    /// `<@id>`, `<@!id>`, `<@&id>`, `<#id>`, `<:name:id>`, `<a:name:id>`, `<t:unix[:S]>`,
    /// `<https://url>`.
    fn angle(&self, i: usize) -> Option<(Inline, usize)> {
        let b = self.src.as_bytes();
        let mut j = i + 1;
        while j < b.len() && j - i <= MAX_ANGLE {
            match b[j] {
                b'>' => break,
                b'<' => return None,
                c if c.is_ascii_whitespace() => return None,
                _ => j += 1,
            }
        }
        if j >= b.len() || b[j] != b'>' {
            return None;
        }
        let inner = &self.src[i + 1..j];
        let end = j + 1;
        let node = if let Some(r) = inner.strip_prefix("@&") {
            Inline::Mention(Mention::Role(parse_id(r)?))
        } else if let Some(r) = inner.strip_prefix('@') {
            let r = r.strip_prefix('!').unwrap_or(r);
            Inline::Mention(Mention::User(parse_id(r)?))
        } else if let Some(r) = inner.strip_prefix('#') {
            Inline::Mention(Mention::Channel(parse_id(r)?))
        } else if let Some(r) = inner.strip_prefix("t:") {
            let (num, style) = match r.split_once(':') {
                Some((n, s)) => {
                    let mut cs = s.chars();
                    let c = cs.next()?;
                    if cs.next().is_some() {
                        return None;
                    }
                    (n, TimestampStyle::from_char(c)?)
                }
                None => (r, TimestampStyle::default()),
            };
            let digits = num.strip_prefix('-').unwrap_or(num);
            if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            Inline::Timestamp {
                unix: num.parse().ok()?,
                style,
            }
        } else if inner.starts_with("https://") || inner.starts_with("http://") {
            if inner.len() <= "https://".len() {
                return None;
            }
            Inline::Link(Link {
                url: inner.to_string(),
                label: None,
                embed: false,
            })
        } else {
            let (animated, r) = match inner.strip_prefix("a:") {
                Some(r) => (true, r),
                None => (false, inner.strip_prefix(':')?),
            };
            let (name, id) = r.split_once(':')?;
            if name.is_empty()
                || name.len() > 32
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return None;
            }
            Inline::CustomEmoji {
                name: name.to_string(),
                id: parse_id(id)?,
                animated,
            }
        };
        Some((node, end))
    }

    /// Bare URL at `i`. Returns (url, end).
    fn url(&self, i: usize) -> Option<(String, usize)> {
        let rest = &self.src[i..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            return None;
        };
        let len = rest
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(rest.len());
        let mut url = &rest[..len];
        // Spoiler markers end a URL.
        if let Some(p) = url[scheme..].find("||") {
            url = &url[..scheme + p];
        }
        // Trailing punctuation is not part of the link (Discord's rule), except a `)`
        // that closes a `(` inside the URL.
        let opens = url.bytes().filter(|&c| c == b'(').count();
        let mut closes = url.bytes().filter(|&c| c == b')').count();
        while let Some(last) = url.chars().next_back() {
            let strip = match last {
                '.' | ',' | ':' | ';' | '"' | '\'' | ']' | '*' | '_' | '~' | '|' | '!' | '?' => {
                    true
                }
                ')' => opens < closes,
                _ => false,
            };
            if !strip || url.len() <= scheme {
                break;
            }
            if last == ')' {
                closes -= 1;
            }
            url = &url[..url.len() - 1];
        }
        if url.len() <= scheme {
            return None;
        }
        Some((url.to_string(), i + url.len()))
    }

    /// `[label](https://url)` or `[label](<https://url>)`.
    fn masked_link(&mut self, i: usize) -> Option<(Inline, usize)> {
        let src = self.src;
        let b = src.as_bytes();
        let mut j = i + 1;
        while j < b.len() && j - i <= MAX_LABEL {
            match b[j] {
                b']' => break,
                b'[' => return None,
                _ => j += 1,
            }
        }
        if j >= b.len() || b[j] != b']' {
            return None;
        }
        let label = &src[i + 1..j];
        if label.trim().is_empty() || !src[j + 1..].starts_with('(') {
            return None;
        }
        let mut k = j + 2;
        let angled = src[k..].starts_with('<');
        if angled {
            k += 1;
        }
        let url_start = k;
        let rest = &src[url_start..];
        if !(rest.starts_with("https://") || rest.starts_with("http://")) {
            return None;
        }
        let stop = if angled { '>' } else { ')' };
        let mut len = 0;
        let mut depth = 0i32;
        for (p, c) in rest.char_indices() {
            if p > MAX_ANGLE || c.is_whitespace() {
                return None;
            }
            if !angled && c == '(' {
                depth += 1;
            } else if c == stop {
                if depth == 0 {
                    len = p;
                    break;
                }
                depth -= 1;
            }
            len = p + c.len_utf8();
        }
        let url_end = url_start + len;
        if src.as_bytes().get(url_end) != Some(&(stop as u8)) || len <= 8 {
            return None;
        }
        let mut end = url_end + 1;
        if angled {
            if b.get(end) != Some(&b')') {
                return None;
            }
            end += 1;
        }
        let label = parse_inline(label, self.depth.saturating_add(1), self.budget);
        Some((
            Inline::Link(Link {
                url: src[url_start..url_end].to_string(),
                label: Some(label),
                embed: !angled,
            }),
            end,
        ))
    }

    // ---- pass 2 ----------------------------------------------------------------------

    fn delim_at(&self, k: usize, hi: usize, ch: u8) -> bool {
        k < hi && matches!(self.toks[k].kind, Kind::Delim(c) if c == ch)
    }

    fn parse_range(&mut self, lo: usize, hi: usize, depth: u8) -> Vec<Inline> {
        let mut out = Vec::new();
        let mut i = lo;
        while i < hi {
            match self.toks[i].kind {
                Kind::Text => {
                    let (s, e) = (self.toks[i].start, self.toks[i].end);
                    push_text(&mut out, &self.src[s..e]);
                    i += 1;
                }
                Kind::Atom(ref mut a) => {
                    if let Some(node) = a.take() {
                        out.push(node);
                    }
                    i += 1;
                }
                Kind::Delim(c) => {
                    if depth < MAX_DEPTH && self.budget.ok() {
                        if let Some((open, close, wrap)) = self.try_match(i, lo, hi, c) {
                            let inner = self.parse_range(i + open, close, depth + 1);
                            out.push(wrap(inner));
                            i = close + open;
                            continue;
                        }
                    }
                    let (s, e) = (self.toks[i].start, self.toks[i].end);
                    push_text(&mut out, &self.src[s..e]);
                    i += 1;
                }
            }
        }
        out
    }

    /// Returns (marker length in tokens, closer index, constructor).
    #[allow(clippy::type_complexity)]
    fn try_match(
        &mut self,
        i: usize,
        lo: usize,
        hi: usize,
        c: u8,
    ) -> Option<(usize, usize, fn(Vec<Inline>) -> Inline)> {
        let double = self.delim_at(i + 1, hi, c);
        match c {
            b'*' => {
                if double {
                    if let Some(k) = self.find_double(i + 2, hi, c, true) {
                        return Some((2, k, Inline::Bold));
                    }
                }
                self.em_star(i, hi)
                    .map(|k| (1, k, Inline::Italic as fn(_) -> _))
            }
            b'_' => {
                if double {
                    if let Some(k) = self.find_double(i + 2, hi, c, true) {
                        return Some((2, k, Inline::Underline));
                    }
                }
                self.em_underscore(i, lo, hi)
                    .map(|k| (1, k, Inline::Italic as fn(_) -> _))
            }
            b'~' if double => self
                .find_double(i + 2, hi, c, false)
                .map(|k| (2, k, Inline::Strike as fn(_) -> _)),
            b'|' if double => self
                .find_double(i + 2, hi, c, false)
                .map(|k| (2, k, Inline::Spoiler as fn(_) -> _)),
            _ => None,
        }
    }

    /// First `cc` at k >= from + 1 (non-empty inner). With `no_third`, the pair must not
    /// be followed by a third `c` (Discord's `(?!\*)` / `(?!_)`).
    fn find_double(&mut self, from: usize, hi: usize, c: u8, no_third: bool) -> Option<usize> {
        // The inner part must hold something besides more of the same marker.
        let mut content = from < hi && !self.delim_at(from, hi, c);
        let mut k = from + 1;
        while k + 1 < hi {
            if !self.budget.tick() {
                return None;
            }
            if content
                && self.delim_at(k, hi, c)
                && self.delim_at(k + 1, hi, c)
                && !(no_third && self.delim_at(k + 2, hi, c))
            {
                return Some(k);
            }
            content |= !self.delim_at(k, hi, c);
            k += 1;
        }
        None
    }

    fn char_before(&self, k: usize) -> Option<char> {
        self.src[..self.toks[k].start].chars().next_back()
    }

    fn char_after(&self, k: usize) -> Option<char> {
        self.src[self.toks[k].end..].chars().next()
    }

    /// `*text*`: no space right after the opener or right before the closer; `**` pairs
    /// inside are skipped (they belong to nested bold).
    fn em_star(&mut self, i: usize, hi: usize) -> Option<usize> {
        let first = i + 1;
        if first >= hi {
            return None;
        }
        if matches!(self.toks[first].kind, Kind::Text)
            && self.src[self.toks[first].start..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            return None;
        }
        let mut k = first;
        let mut content = false;
        while k < hi {
            if !self.budget.tick() {
                return None;
            }
            if self.delim_at(k, hi, b'*') {
                if self.delim_at(k + 1, hi, b'*') {
                    k += 2;
                    continue;
                }
                if !content || self.char_before(k).is_some_and(char::is_whitespace) {
                    return None;
                }
                return Some(k);
            }
            content = true;
            k += 1;
        }
        None
    }

    /// `_text_`: word boundaries on both outer sides (so snake_case stays plain).
    fn em_underscore(&mut self, i: usize, lo: usize, hi: usize) -> Option<usize> {
        if i > lo && self.char_before(i).is_some_and(is_word) {
            return None;
        }
        let first = i + 1;
        let mut k = first;
        while k < hi {
            if !self.budget.tick() {
                return None;
            }
            if self.delim_at(k, hi, b'_') {
                if self.delim_at(k + 1, hi, b'_') {
                    k += 2;
                    continue;
                }
                if k == first {
                    return None;
                }
                if k + 1 < hi && self.char_after(k).is_some_and(is_word) {
                    return None;
                }
                return Some(k);
            }
            k += 1;
        }
        None
    }
}

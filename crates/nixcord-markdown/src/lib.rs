//! Discord-flavored markdown, parsed into a small AST.
//!
//! Pure: no UI dependencies. The app crate renders the AST with egui.
//!
//! ```
//! use nixcord_markdown::{parse, Block, Inline};
//!
//! let doc = parse("**hi** <@42>");
//! let Block::Paragraph(inl) = &doc.blocks[0] else { panic!() };
//! assert_eq!(inl[0], Inline::Bold(vec![Inline::Text("hi".into())]));
//! ```
//!
//! Guarantees:
//! - never panics, whatever the input;
//! - roughly linear time: every scan is bounded, and the emphasis matcher runs on a work
//!   budget proportional to the input length, after which remaining markers stay literal;
//! - unclosed markers stay literal text; code spans and code blocks contain no formatting.

mod block;
mod highlight;
mod inline;
#[cfg(test)]
mod tests;

pub use highlight::{highlight, HlKind, HlSpan};

/// A parsed message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// One or more lines of inline content. Line breaks are kept as `\n` inside `Text`.
    Paragraph(Vec<Inline>),
    /// `# `, `## `, `### ` (level 1..=3).
    Heading { level: u8, content: Vec<Inline> },
    /// `-# small muted text`.
    Subtext(Vec<Inline>),
    /// `> line` (consecutive lines merge) or `>>> rest of message`. Never nested.
    Quote(Vec<Block>),
    /// Fenced code block.
    CodeBlock(CodeBlock),
    /// `- item`, `* item`, `1. item`; deeper indentation nests.
    List(List),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    /// Language tag as written (`rust`, `py`, ...), if any.
    pub lang: Option<String>,
    pub code: String,
    /// Syntax highlight spans (byte ranges into `code`), computed once at parse time.
    /// Empty for unknown languages.
    pub highlights: Vec<HlSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub ordered: bool,
    /// Number of the first item (ordered lists only; 1 for bullets).
    pub start: u64,
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub content: Vec<Inline>,
    /// Nested lists (more indented lines that follow this item).
    pub children: Vec<List>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// Plain text; may contain `\n`. Adjacent text is always merged.
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Underline(Vec<Inline>),
    Strike(Vec<Inline>),
    Spoiler(Vec<Inline>),
    /// Inline code (no formatting inside).
    Code(String),
    Link(Link),
    Mention(Mention),
    /// `<:name:id>` / `<a:name:id>`.
    CustomEmoji {
        name: String,
        id: u64,
        animated: bool,
    },
    /// `<t:unix>` / `<t:unix:style>`.
    Timestamp {
        unix: i64,
        style: TimestampStyle,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    /// `Some` for masked links `[label](url)`; `None` for bare / `<url>` links.
    pub label: Option<Vec<Inline>>,
    /// False for `<https://...>` (embed suppressed).
    pub embed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mention {
    User(u64),
    Role(u64),
    Channel(u64),
    Everyone,
    Here,
}

/// `<t:...:X>` styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TimestampStyle {
    /// `t` 16:20
    ShortTime,
    /// `T` 16:20:30
    LongTime,
    /// `d` 20/04/2021
    ShortDate,
    /// `D` 20 April 2021
    LongDate,
    /// `f` (default) 20 April 2021 16:20
    #[default]
    ShortDateTime,
    /// `F` Tuesday, 20 April 2021 16:20
    LongDateTime,
    /// `R` 2 months ago
    Relative,
}

impl TimestampStyle {
    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            't' => Self::ShortTime,
            'T' => Self::LongTime,
            'd' => Self::ShortDate,
            'D' => Self::LongDate,
            'f' => Self::ShortDateTime,
            'F' => Self::LongDateTime,
            'R' => Self::Relative,
            _ => return None,
        })
    }

    pub fn as_char(self) -> char {
        match self {
            Self::ShortTime => 't',
            Self::LongTime => 'T',
            Self::ShortDate => 'd',
            Self::LongDate => 'D',
            Self::ShortDateTime => 'f',
            Self::LongDateTime => 'F',
            Self::Relative => 'R',
        }
    }
}

/// Work budget shared by one `parse` call.
pub(crate) struct Budget(usize);

impl Budget {
    fn for_len(len: usize) -> Self {
        Self(len.saturating_mul(64).saturating_add(1024))
    }
    /// Spend one unit; false once exhausted.
    #[inline]
    pub(crate) fn tick(&mut self) -> bool {
        if self.0 == 0 {
            false
        } else {
            self.0 -= 1;
            true
        }
    }
    #[inline]
    pub(crate) fn ok(&self) -> bool {
        self.0 > 0
    }
}

/// Parse a message's content.
pub fn parse(src: &str) -> Document {
    let normalized;
    let src = if src.contains('\r') {
        normalized = src.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        src
    };
    let mut budget = Budget::for_len(src.len());
    Document {
        blocks: block::parse_blocks(src, false, &mut budget),
    }
}

/// Parse inline content only (no blocks): handy for one-line previews.
pub fn parse_inline(src: &str) -> Vec<Inline> {
    let mut budget = Budget::for_len(src.len());
    inline::parse_inline(src, 0, &mut budget)
}

impl Document {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Visit every inline node (depth first, including nested ones and link labels).
    pub fn walk(&self, f: &mut dyn FnMut(&Inline)) {
        fn inlines(v: &[Inline], f: &mut dyn FnMut(&Inline)) {
            for i in v {
                f(i);
                match i {
                    Inline::Bold(c)
                    | Inline::Italic(c)
                    | Inline::Underline(c)
                    | Inline::Strike(c)
                    | Inline::Spoiler(c) => inlines(c, f),
                    Inline::Link(Link { label: Some(c), .. }) => inlines(c, f),
                    _ => {}
                }
            }
        }
        fn list(l: &List, f: &mut dyn FnMut(&Inline)) {
            for it in &l.items {
                inlines(&it.content, f);
                for c in &it.children {
                    list(c, f);
                }
            }
        }
        fn blocks(v: &[Block], f: &mut dyn FnMut(&Inline)) {
            for b in v {
                match b {
                    Block::Paragraph(c) | Block::Subtext(c) | Block::Heading { content: c, .. } => {
                        inlines(c, f)
                    }
                    Block::Quote(b) => blocks(b, f),
                    Block::List(l) => list(l, f),
                    Block::CodeBlock(_) => {}
                }
            }
        }
        blocks(&self.blocks, f);
    }

    /// Does the content mention this user (directly)?
    pub fn mentions_user(&self, id: u64) -> bool {
        self.any(|i| matches!(i, Inline::Mention(Mention::User(u)) if *u == id))
    }

    /// `@everyone` or `@here` outside code.
    pub fn mentions_everyone(&self) -> bool {
        self.any(|i| matches!(i, Inline::Mention(Mention::Everyone | Mention::Here)))
    }

    fn any(&self, pred: impl Fn(&Inline) -> bool) -> bool {
        let mut hit = false;
        self.walk(&mut |i| hit = hit || pred(i));
        hit
    }

    /// Only emoji (custom or unicode) and whitespace, at most 30 of them: Discord shows
    /// these large.
    pub fn is_emoji_only(&self) -> bool {
        let [Block::Paragraph(inl)] = self.blocks.as_slice() else {
            return false;
        };
        let mut n = 0;
        for i in inl {
            match i {
                Inline::CustomEmoji { .. } => n += 1,
                Inline::Text(t) => match nixcord_emoji::count_emoji_only(t) {
                    Some(k) => n += k,
                    None => return false,
                },
                _ => return false,
            }
        }
        (1..=30).contains(&n)
    }

    /// Flattened text with formatting removed; `mention` names mentions, emoji and
    /// timestamps. Blocks are separated by newlines.
    pub fn plain_text_with(&self, mention: &dyn Fn(&Inline) -> String) -> String {
        fn inlines(v: &[Inline], out: &mut String, m: &dyn Fn(&Inline) -> String) {
            for i in v {
                match i {
                    Inline::Text(t) | Inline::Code(t) => out.push_str(t),
                    Inline::Bold(c)
                    | Inline::Italic(c)
                    | Inline::Underline(c)
                    | Inline::Strike(c)
                    | Inline::Spoiler(c) => inlines(c, out, m),
                    Inline::Link(Link { label: Some(c), .. }) => inlines(c, out, m),
                    Inline::Link(Link { url, .. }) => out.push_str(url),
                    other => out.push_str(&m(other)),
                }
            }
        }
        fn blocks(v: &[Block], out: &mut String, m: &dyn Fn(&Inline) -> String) {
            for b in v {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                match b {
                    Block::Paragraph(c) | Block::Subtext(c) | Block::Heading { content: c, .. } => {
                        inlines(c, out, m)
                    }
                    Block::Quote(b) => blocks(b, out, m),
                    Block::CodeBlock(c) => out.push_str(&c.code),
                    Block::List(l) => list(l, out, m),
                }
            }
        }
        fn list(l: &List, out: &mut String, m: &dyn Fn(&Inline) -> String) {
            for it in &l.items {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                inlines(&it.content, out, m);
                for c in &it.children {
                    list(c, out, m);
                }
            }
        }
        let mut out = String::new();
        blocks(&self.blocks, &mut out, mention);
        out
    }
}

//! Block structure: paragraphs, headings, subtext, quotes, fenced code, lists.

use crate::inline::parse_inline;
use crate::{highlight, Block, Budget, CodeBlock, List, ListItem};

/// Lists nest at most this deep; deeper lines become siblings.
const MAX_LIST_DEPTH: usize = 8;

pub(crate) fn parse_blocks(src: &str, in_quote: bool, budget: &mut Budget) -> Vec<Block> {
    let mut p = Parser {
        src,
        in_quote,
        budget,
        out: Vec::new(),
        para: None,
        no_more_fences: false,
    };
    p.run();
    p.out
}

struct Parser<'a, 'b> {
    src: &'a str,
    in_quote: bool,
    budget: &'b mut Budget,
    out: Vec<Block>,
    /// Byte range of the paragraph being collected.
    para: Option<(usize, usize)>,
    /// A previous fence search found no closing fence: no later one can succeed.
    no_more_fences: bool,
}

struct Fence {
    open: usize,
    lang: Option<String>,
    code: String,
    /// Byte just past the closing fence.
    end: usize,
}

struct ListLine<'a> {
    indent: usize,
    ordered: bool,
    num: u64,
    text: &'a str,
}

fn line_end(src: &str, pos: usize) -> usize {
    src[pos..].find('\n').map_or(src.len(), |i| pos + i)
}

impl Parser<'_, '_> {
    fn run(&mut self) {
        let src = self.src;
        let mut pos = 0;
        while pos < src.len() {
            let end = line_end(src, pos);
            let next = (end + 1).min(src.len());
            let at_line_start = pos == 0 || src.as_bytes()[pos - 1] == b'\n';
            if at_line_start {
                if let Some(np) = self.block_at(pos, end) {
                    pos = np;
                    continue;
                }
            }
            if let Some(f) = self.find_fence(pos, end) {
                self.extend_para(pos, f.open);
                self.flush_para();
                let highlights = f
                    .lang
                    .as_deref()
                    .map(|l| highlight(l, &f.code))
                    .unwrap_or_default();
                self.out.push(Block::CodeBlock(CodeBlock {
                    lang: f.lang,
                    code: f.code,
                    highlights,
                }));
                pos = f.end;
                // The line break right after a closing fence belongs to the block.
                if src.as_bytes().get(pos) == Some(&b'\n') {
                    pos += 1;
                }
                continue;
            }
            self.extend_para(pos, next);
            pos = next;
        }
        self.flush_para();
    }

    fn extend_para(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        self.para = Some(match self.para {
            Some((s, _)) => (s, to),
            None => (from, to),
        });
    }

    fn flush_para(&mut self) {
        let Some((s, e)) = self.para.take() else {
            return;
        };
        let mut text = &self.src[s..e];
        if let Some(t) = text.strip_suffix('\n') {
            text = t;
        }
        if text.is_empty() {
            return;
        }
        let inl = parse_inline(text, 0, self.budget);
        if !inl.is_empty() {
            self.out.push(Block::Paragraph(inl));
        }
    }

    /// Try a line-start block construct on the line `[pos, end)`. Returns the position
    /// after it when one was consumed.
    fn block_at(&mut self, pos: usize, end: usize) -> Option<usize> {
        let src = self.src;
        let line = &src[pos..end];
        let trimmed = line.trim_start_matches(' ');

        if !self.in_quote {
            if let Some(rest) = trimmed.strip_prefix(">>> ") {
                self.flush_para();
                let start = end - rest.len();
                let inner = parse_blocks(&src[start..], true, self.budget);
                self.out.push(Block::Quote(inner));
                return Some(src.len());
            }
            if quote_line(trimmed).is_some() {
                self.flush_para();
                let mut joined = String::new();
                let mut p = pos;
                while p < src.len() {
                    let e = line_end(src, p);
                    let Some(content) = quote_line(src[p..e].trim_start_matches(' ')) else {
                        break;
                    };
                    if !joined.is_empty() || p != pos {
                        joined.push('\n');
                    }
                    joined.push_str(content);
                    p = (e + 1).min(src.len());
                    if e == src.len() {
                        break;
                    }
                }
                let inner = parse_blocks(&joined, true, self.budget);
                self.out.push(Block::Quote(inner));
                return Some(p);
            }
        }

        for (level, marker) in [(1u8, "# "), (2, "## "), (3, "### ")] {
            if let Some(rest) = line.strip_prefix(marker) {
                if rest.trim().is_empty() {
                    return None;
                }
                self.flush_para();
                let content = parse_inline(rest.trim(), 0, self.budget);
                self.out.push(Block::Heading { level, content });
                return Some((end + 1).min(src.len()));
            }
        }
        if let Some(rest) = line.strip_prefix("-# ") {
            if rest.trim().is_empty() {
                return None;
            }
            self.flush_para();
            let content = parse_inline(rest.trim(), 0, self.budget);
            self.out.push(Block::Subtext(content));
            return Some((end + 1).min(src.len()));
        }

        list_line(line)?;
        self.flush_para();
        let mut lines = Vec::new();
        let mut p = pos;
        while p < src.len() {
            let e = line_end(src, p);
            let Some(l) = list_line(&src[p..e]) else {
                break;
            };
            lines.push(l);
            p = (e + 1).min(src.len());
        }
        let mut i = 0;
        while i < lines.len() {
            let list = self.build_list(&lines, &mut i, 0);
            self.out.push(Block::List(list));
        }
        Some(p)
    }

    fn build_list(&mut self, lines: &[ListLine<'_>], i: &mut usize, depth: usize) -> List {
        let first = &lines[*i];
        let indent0 = first.indent;
        let ordered = first.ordered;
        let mut list = List {
            ordered,
            start: if ordered { first.num } else { 1 },
            items: Vec::new(),
        };
        while *i < lines.len() {
            let l = &lines[*i];
            if l.indent < indent0 && !list.items.is_empty() {
                break;
            }
            if l.indent > indent0 && !list.items.is_empty() && depth < MAX_LIST_DEPTH {
                let child = self.build_list(lines, i, depth + 1);
                if let Some(last) = list.items.last_mut() {
                    last.children.push(child);
                }
                continue;
            }
            if l.ordered != ordered && !list.items.is_empty() {
                break;
            }
            let content = parse_inline(l.text, 0, self.budget);
            list.items.push(ListItem {
                content,
                children: Vec::new(),
            });
            *i += 1;
        }
        list
    }

    /// Find an opening ``` in `[from, line_end)` that has a closing ``` somewhere later.
    fn find_fence(&mut self, from: usize, line_end: usize) -> Option<Fence> {
        if self.no_more_fences {
            return None;
        }
        let src = self.src;
        let b = src.as_bytes();
        let mut i = from;
        while i < line_end {
            match b[i] {
                b'\\' => i += 2,
                b'`' => {
                    let run = b[i..line_end].iter().take_while(|&&c| c == b'`').count();
                    if run < 3 {
                        i += run;
                        continue;
                    }
                    let Some(close) = src[i + 3..].find("```").map(|c| i + 3 + c) else {
                        self.no_more_fences = true;
                        return None;
                    };
                    let body = &src[i + 3..close];
                    let (lang, code) = split_lang(body);
                    let code = code.trim_start_matches('\n').trim_end_matches('\n');
                    if code.is_empty() {
                        i += run;
                        continue;
                    }
                    return Some(Fence {
                        open: i,
                        lang: lang.map(str::to_string),
                        code: code.to_string(),
                        end: close + 3,
                    });
                }
                _ => i += 1,
            }
        }
        None
    }
}

/// `lang\n...` -> (Some(lang), rest) when the first line is a bare language tag.
fn split_lang(body: &str) -> (Option<&str>, &str) {
    if let Some(nl) = body.find('\n') {
        let tag = &body[..nl];
        let rest = &body[nl + 1..];
        let valid = !tag.is_empty()
            && tag.len() <= 32
            && tag
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_+-.#".contains(&c));
        if valid && !rest.trim().is_empty() {
            return (Some(tag), rest);
        }
    }
    (None, body)
}

/// `> text` -> Some("text").
fn quote_line(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with(">>> ") {
        return None;
    }
    trimmed.strip_prefix("> ")
}

fn list_line(line: &str) -> Option<ListLine<'_>> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let (ordered, num, text) = if let Some(t) = rest.strip_prefix("- ").or(rest.strip_prefix("* "))
    {
        (false, 1, t)
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let t = rest[digits..].strip_prefix(". ")?;
        (true, rest[..digits].parse().ok()?, t)
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(ListLine {
        indent,
        ordered,
        num,
        text: text.trim_end(),
    })
}

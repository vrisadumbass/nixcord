//! Tiny single-pass syntax highlighter for code blocks (rust, js, ts, py, json, toml).
//!
//! Not a real lexer: keywords, strings, numbers, comments, type-ish names, keys. Linear
//! time, never panics, unknown languages get no spans.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlKind {
    Keyword,
    String,
    Number,
    Comment,
    /// Capitalized identifiers, TOML `[tables]`.
    Type,
    /// `true` / `false` / `null` / `None` ...
    Literal,
    /// JSON object keys, TOML keys.
    Key,
}

/// Byte range into the code plus its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlSpan {
    pub start: u32,
    pub end: u32,
    pub kind: HlKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Js,
    Py,
    Json,
    Toml,
}

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while", "yield",
];
const RUST_LIT: &[&str] = &["true", "false", "None", "Some", "Ok", "Err"];
const JS_KW: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "yield",
    "as",
    "declare",
    "namespace",
    "keyof",
];
const JS_LIT: &[&str] = &["true", "false", "null", "undefined", "NaN", "Infinity"];
const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "self",
    "match", "case",
];
const PY_LIT: &[&str] = &["True", "False", "None"];
const DATA_LIT: &[&str] = &["true", "false", "null"];

fn lang_of(tag: &str) -> Option<Lang> {
    Some(match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "ts" | "typescript" | "tsx" => Lang::Js,
        "py" | "python" | "py3" => Lang::Py,
        "json" | "jsonc" | "json5" => Lang::Json,
        "toml" => Lang::Toml,
        _ => return None,
    })
}

/// Highlight `code` written in `lang` (a code fence tag such as `rust` or `py`).
pub fn highlight(lang: &str, code: &str) -> Vec<HlSpan> {
    let Some(lang) = lang_of(lang) else {
        return Vec::new();
    };
    let b = code.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut push = |s: usize, e: usize, kind| {
        if s < e {
            out.push(HlSpan {
                start: s as u32,
                end: e as u32,
                kind,
            })
        }
    };
    let line_comment: &[u8] = match lang {
        Lang::Rust | Lang::Js | Lang::Json => b"//",
        Lang::Py | Lang::Toml => b"#",
    };
    let block_comments = matches!(lang, Lang::Rust | Lang::Js | Lang::Json);
    let mut line_start = true;
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        let at_line_start = std::mem::replace(&mut line_start, false);
        // comments
        if b[i..].starts_with(line_comment) {
            let e = memchr(b, i, b'\n');
            push(i, e, HlKind::Comment);
            i = e;
            continue;
        }
        if block_comments && b[i..].starts_with(b"/*") {
            let e = find(b, i + 2, b"*/").map_or(n, |p| p + 2);
            push(i, e, HlKind::Comment);
            i = e;
            continue;
        }
        // TOML table headers
        if lang == Lang::Toml && at_line_start && c == b'[' {
            let e = memchr(b, i, b'\n');
            let close = b[i..e]
                .iter()
                .rposition(|&x| x == b']')
                .map_or(e, |p| i + p + 1);
            push(i, close, HlKind::Type);
            i = close;
            continue;
        }
        // strings
        let is_quote =
            c == b'"' || (c == b'\'' && lang != Lang::Json) || (c == b'`' && lang == Lang::Js);
        if is_quote {
            if lang == Lang::Rust && c == b'\'' {
                // char literal ('a', '\n') vs lifetime ('a)
                let lit = match b.get(i + 1) {
                    Some(b'\\') => true,
                    Some(_) => {
                        let ch_len = code[i + 1..].chars().next().map_or(1, char::len_utf8);
                        b.get(i + 1 + ch_len) == Some(&b'\'')
                    }
                    None => false,
                };
                if !lit {
                    i += 1;
                    continue;
                }
            }
            let triple = lang == Lang::Py || lang == Lang::Toml;
            let e = if triple && b[i..].starts_with(&[c, c, c]) {
                find(b, i + 3, &[c, c, c]).map_or(n, |p| p + 3)
            } else {
                string_end(b, i, c, c == b'`')
            };
            let is_key = match lang {
                Lang::Json => next_non_space(b, e) == Some(b':'),
                Lang::Toml => next_non_space(b, e) == Some(b'='),
                _ => false,
            };
            push(i, e, if is_key { HlKind::Key } else { HlKind::String });
            i = e;
            continue;
        }
        // numbers
        if c.is_ascii_digit() {
            let mut e = i + 1;
            while e < n && (b[e].is_ascii_alphanumeric() || b[e] == b'_' || b[e] == b'.') {
                if b[e] == b'.' && !b.get(e + 1).is_some_and(u8::is_ascii_digit) {
                    break;
                }
                e += 1;
            }
            push(i, e, HlKind::Number);
            i = e;
            continue;
        }
        // identifiers
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let mut e = i + 1;
            while e < n && (b[e].is_ascii_alphanumeric() || b[e] == b'_' || b[e] == b'$') {
                e += 1;
            }
            let word = &code[i..e];
            let (kw, lit): (&[&str], &[&str]) = match lang {
                Lang::Rust => (RUST_KW, RUST_LIT),
                Lang::Js => (JS_KW, JS_LIT),
                Lang::Py => (PY_KW, PY_LIT),
                Lang::Json | Lang::Toml => (&[], DATA_LIT),
            };
            let kind = if lit.contains(&word) {
                Some(HlKind::Literal)
            } else if kw.contains(&word) {
                Some(HlKind::Keyword)
            } else if lang == Lang::Toml
                && at_line_start
                && matches!(next_non_space(b, e), Some(b'=' | b'.'))
            {
                Some(HlKind::Key)
            } else if matches!(lang, Lang::Rust | Lang::Js | Lang::Py)
                && c.is_ascii_uppercase()
                && word.len() > 1
            {
                Some(HlKind::Type)
            } else {
                None
            };
            // Rust macros: `println!`
            if lang == Lang::Rust && b.get(e) == Some(&b'!') && kind.is_none() {
                push(i, e + 1, HlKind::Keyword);
                i = e + 1;
                continue;
            }
            if let Some(k) = kind {
                push(i, e, k);
            }
            i = e;
            continue;
        }
        // anything else: skip one char
        i += code[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn memchr(b: &[u8], from: usize, c: u8) -> usize {
    b[from..]
        .iter()
        .position(|&x| x == c)
        .map_or(b.len(), |p| from + p)
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from > b.len() {
        return None;
    }
    b[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| from + p)
}

/// End (exclusive) of a string starting with quote `q` at `i`. Single-line unless
/// `multiline`; unterminated strings run to the end of the line.
fn string_end(b: &[u8], i: usize, q: u8, multiline: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'\n' if !multiline => return j,
            x if x == q => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

fn next_non_space(b: &[u8], from: usize) -> Option<u8> {
    b.get(from..)?
        .iter()
        .copied()
        .find(|c| *c != b' ' && *c != b'\t')
}

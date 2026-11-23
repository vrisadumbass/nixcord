use crate::*;

fn t(s: &str) -> Inline {
    Inline::Text(s.into())
}
fn b(v: Vec<Inline>) -> Inline {
    Inline::Bold(v)
}
fn it(v: Vec<Inline>) -> Inline {
    Inline::Italic(v)
}
fn u(v: Vec<Inline>) -> Inline {
    Inline::Underline(v)
}
fn code(s: &str) -> Inline {
    Inline::Code(s.into())
}
fn link(url: &str) -> Inline {
    Inline::Link(Link {
        url: url.into(),
        label: None,
        embed: true,
    })
}

/// Inline content of a single-paragraph message.
fn inl(src: &str) -> Vec<Inline> {
    let doc = parse(src);
    match doc.blocks.as_slice() {
        [Block::Paragraph(v)] => v.clone(),
        other => panic!("expected one paragraph for {src:?}, got {other:?}"),
    }
}

fn blocks(src: &str) -> Vec<Block> {
    parse(src).blocks
}

// ---- plain text ---------------------------------------------------------------------

#[test]
fn empty_and_plain() {
    assert!(parse("").is_empty());
    assert_eq!(inl("hello world"), vec![t("hello world")]);
    assert_eq!(inl("a\nb\n\nc"), vec![t("a\nb\n\nc")]);
    assert_eq!(inl("   "), vec![t("   ")]);
}

#[test]
fn crlf_is_normalized() {
    assert_eq!(inl("a\r\nb"), vec![t("a\nb")]);
}

// ---- emphasis -----------------------------------------------------------------------

#[test]
fn bold_italic_underline_strike_spoiler() {
    assert_eq!(inl("**b**"), vec![b(vec![t("b")])]);
    assert_eq!(inl("*i*"), vec![it(vec![t("i")])]);
    assert_eq!(inl("_i_"), vec![it(vec![t("i")])]);
    assert_eq!(inl("__u__"), vec![u(vec![t("u")])]);
    assert_eq!(inl("~~s~~"), vec![Inline::Strike(vec![t("s")])]);
    assert_eq!(inl("||sp||"), vec![Inline::Spoiler(vec![t("sp")])]);
}

#[test]
fn text_around_emphasis() {
    assert_eq!(inl("a **b** c"), vec![t("a "), b(vec![t("b")]), t(" c")]);
}

#[test]
fn nesting() {
    assert_eq!(
        inl("*a **b** c*"),
        vec![it(vec![t("a "), b(vec![t("b")]), t(" c")])]
    );
    assert_eq!(
        inl("**a *b* c**"),
        vec![b(vec![t("a "), it(vec![t("b")]), t(" c")])]
    );
    assert_eq!(inl("__**_x_**__"), vec![u(vec![b(vec![it(vec![t("x")])])])]);
    assert_eq!(
        inl("||**secret** stuff||"),
        vec![Inline::Spoiler(vec![b(vec![t("secret")]), t(" stuff")])]
    );
    assert_eq!(
        inl("~~*gone*~~"),
        vec![Inline::Strike(vec![it(vec![t("gone")])])]
    );
}

#[test]
fn triple_star_is_bold_italic() {
    let v = inl("***x***");
    let ok = v == vec![b(vec![it(vec![t("x")])])] || v == vec![it(vec![b(vec![t("x")])])];
    assert!(ok, "{v:?}");
}

#[test]
fn underline_italic_combo() {
    let v = inl("___x___");
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(matches!(v[0], Inline::Underline(_) | Inline::Italic(_)));
}

#[test]
fn unclosed_markers_stay_literal() {
    assert_eq!(inl("**bold"), vec![t("**bold")]);
    assert_eq!(inl("*it"), vec![t("*it")]);
    assert_eq!(inl("~~x"), vec![t("~~x")]);
    assert_eq!(inl("||x"), vec![t("||x")]);
    assert_eq!(inl("__x"), vec![t("__x")]);
    assert_eq!(inl("`x"), vec![t("`x")]);
    assert_eq!(inl("a * b * c"), vec![t("a * b * c")]);
    assert_eq!(inl("2*3*4"), vec![t("2"), it(vec![t("3")]), t("4")]);
}

#[test]
fn empty_markers_are_literal() {
    assert_eq!(inl("****"), vec![t("****")]);
    assert_eq!(inl("~~~~"), vec![t("~~~~")]);
    assert_eq!(inl("||||"), vec![t("||||")]);
    assert_eq!(inl("**"), vec![t("**")]);
    assert_eq!(inl("*"), vec![t("*")]);
}

#[test]
fn star_italic_needs_non_space_edges() {
    assert_eq!(inl("a * not italic*"), vec![t("a * not italic*")]);
    // At line start `* ` is a list item instead.
    assert!(matches!(blocks("* not italic*")[0], Block::List(_)));
    assert_eq!(inl("*not italic *"), vec![t("*not italic *")]);
    assert_eq!(inl("*yes it is*"), vec![it(vec![t("yes it is")])]);
}

#[test]
fn snake_case_is_not_italic() {
    assert_eq!(inl("snake_case_name"), vec![t("snake_case_name")]);
    assert_eq!(inl("a_b c_d"), vec![t("a_b c_d")]);
    assert_eq!(
        inl("_ok_ but not_this_"),
        vec![it(vec![t("ok")]), t(" but not_this_")]
    );
    assert_eq!(
        inl(":thumbs_up: :raised_hand:"),
        vec![t(":thumbs_up: :raised_hand:")]
    );
}

#[test]
fn first_closer_wins() {
    assert_eq!(inl("**a**b**"), vec![b(vec![t("a")]), t("b**")]);
    assert_eq!(
        inl("||a|| ||b||"),
        vec![
            Inline::Spoiler(vec![t("a")]),
            t(" "),
            Inline::Spoiler(vec![t("b")])
        ]
    );
}

#[test]
fn emphasis_spans_lines() {
    assert_eq!(inl("**a\nb**"), vec![b(vec![t("a\nb")])]);
}

#[test]
fn bold_closer_not_followed_by_star() {
    // `**a***` -> bold("a*")
    assert_eq!(inl("**a***"), vec![b(vec![t("a*")])]);
}

// ---- escapes ------------------------------------------------------------------------

#[test]
fn escapes() {
    assert_eq!(inl(r"\*not\*"), vec![t("*not*")]);
    assert_eq!(inl(r"\*\*x\*\*"), vec![t("**x**")]);
    assert_eq!(inl(r"\_x\_"), vec![t("_x_")]);
    assert_eq!(inl(r"\`x\`"), vec![t("`x`")]);
    assert_eq!(inl(r"\\"), vec![t("\\")]);
    assert_eq!(inl(r"\<@123>"), vec![t("<@123>")]);
    // Backslash before alphanumerics / whitespace is literal.
    assert_eq!(inl(r"C:\Users\x"), vec![t(r"C:\Users\x")]);
    assert_eq!(inl(r"end\"), vec![t(r"end\")]);
    assert_eq!(
        inl("**a\\**b**"),
        vec![b(vec![t("a*"), t("*b")]
            .into_iter()
            .fold(Vec::new(), merge))]
    );
}

fn merge(mut v: Vec<Inline>, i: Inline) -> Vec<Inline> {
    if let (Some(Inline::Text(a)), Inline::Text(b)) = (v.last_mut(), &i) {
        a.push_str(b);
    } else {
        v.push(i);
    }
    v
}

// ---- code ---------------------------------------------------------------------------

#[test]
fn inline_code() {
    assert_eq!(inl("`x`"), vec![code("x")]);
    assert_eq!(
        inl("use `a*b*c` here"),
        vec![t("use "), code("a*b*c"), t(" here")]
    );
    assert_eq!(inl("`` a`b ``"), vec![code("a`b")]);
    assert_eq!(inl("``a`b``"), vec![code("a`b")]);
    assert_eq!(inl("`<@123> **x**`"), vec![code("<@123> **x**")]);
    assert_eq!(inl(r"`a\`"), vec![code(r"a\")]);
}

#[test]
fn code_is_opaque_to_emphasis() {
    assert_eq!(
        inl("*a `*` b*"),
        vec![it(vec![t("a "), code("*"), t(" b")])]
    );
    assert_eq!(
        inl("**see `a**b`**"),
        vec![b(vec![t("see "), code("a**b")])]
    );
}

#[test]
fn code_block_basic() {
    let v = blocks("```rust\nfn main() {}\n```");
    let [Block::CodeBlock(c)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(c.lang.as_deref(), Some("rust"));
    assert_eq!(c.code, "fn main() {}");
    assert!(!c.highlights.is_empty());
}

#[test]
fn code_block_no_lang_and_inline_form() {
    let v = blocks("```hello```");
    let [Block::CodeBlock(c)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(c.lang, None);
    assert_eq!(c.code, "hello");

    let v = blocks("```\nplain text\n```");
    let [Block::CodeBlock(c)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(c.lang, None);
    assert_eq!(c.code, "plain text");
}

#[test]
fn code_block_single_word_is_code_not_lang() {
    let v = blocks("```rust\n```");
    // "rust" with nothing after: the word is the content.
    let [Block::CodeBlock(c)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(c.code, "rust");
    assert_eq!(c.lang, None);
}

#[test]
fn code_block_contains_no_formatting() {
    let v = blocks("```\n**not bold** <@1> # nope\n> no quote\n```");
    let [Block::CodeBlock(c)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(c.code, "**not bold** <@1> # nope\n> no quote");
}

#[test]
fn code_block_mid_paragraph() {
    let v = blocks("before ```mid``` after");
    assert_eq!(v.len(), 3, "{v:?}");
    assert_eq!(v[0], Block::Paragraph(vec![t("before ")]));
    assert!(matches!(&v[1], Block::CodeBlock(c) if c.code == "mid"));
    assert_eq!(v[2], Block::Paragraph(vec![t(" after")]));
}

#[test]
fn code_block_between_lines() {
    let v = blocks("look:\n```js\nlet x = 1;\n```\ncool");
    assert_eq!(v.len(), 3, "{v:?}");
    assert_eq!(v[0], Block::Paragraph(vec![t("look:")]));
    assert!(matches!(&v[1], Block::CodeBlock(c) if c.lang.as_deref() == Some("js")));
    assert_eq!(v[2], Block::Paragraph(vec![t("cool")]));
}

#[test]
fn unclosed_code_block_is_literal() {
    assert_eq!(inl("```rust\nfn x"), vec![t("```rust\nfn x")]);
    // A later closed fence still works after a failed search.
    let v = blocks("```a``` and ```");
    assert!(
        matches!(&v[0], Block::CodeBlock(c) if c.code == "a"),
        "{v:?}"
    );
}

#[test]
fn escaped_fence() {
    let v = blocks("\\```x```");
    assert!(
        !v.iter()
            .any(|b| matches!(b, Block::CodeBlock(c) if c.code == "x")),
        "{v:?}"
    );
}

#[test]
fn code_block_in_quote() {
    let v = blocks("> ```\n> quoted code\n> ```");
    let [Block::Quote(inner)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert!(matches!(&inner[0], Block::CodeBlock(c) if c.code == "quoted code"));
}

// ---- links --------------------------------------------------------------------------

#[test]
fn bare_urls() {
    assert_eq!(inl("https://x.com"), vec![link("https://x.com")]);
    assert_eq!(
        inl("go to https://x.com/a?b=1."),
        vec![t("go to "), link("https://x.com/a?b=1"), t(".")]
    );
    assert_eq!(
        inl("(https://x.com)"),
        vec![t("("), link("https://x.com"), t(")")]
    );
    assert_eq!(
        inl("https://en.wikipedia.org/wiki/Rust_(programming_language)"),
        vec![link(
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        )]
    );
    assert_eq!(
        inl("see https://a.b, https://c.d;"),
        vec![
            t("see "),
            link("https://a.b"),
            t(", "),
            link("https://c.d"),
            t(";")
        ]
    );
    assert_eq!(inl("http://old.site"), vec![link("http://old.site")]);
    assert_eq!(inl("https://"), vec![t("https://")]);
    assert_eq!(inl("xhttps://nope.com"), vec![t("xhttps://nope.com")]);
}

#[test]
fn url_keeps_underscores_and_stars_inside() {
    assert_eq!(
        inl("https://x.com/a_b_c*d"),
        vec![link("https://x.com/a_b_c*d")]
    );
    assert_eq!(
        // The URL ends before the trailing `_`, which then closes the italics.
        inl("_https://x.com/a_b_ x_"),
        vec![it(vec![link("https://x.com/a_b")]), t(" x_")]
    );
}

#[test]
fn url_inside_bold_and_spoiler() {
    assert_eq!(
        inl("**https://x.com**"),
        vec![b(vec![link("https://x.com")])]
    );
    assert_eq!(
        inl("||https://x.com||"),
        vec![Inline::Spoiler(vec![link("https://x.com")])]
    );
}

#[test]
fn angle_url_suppresses_embed() {
    assert_eq!(
        inl("<https://x.com/a b>"),
        vec![t("<"), link("https://x.com/a"), t(" b>")]
    );
    assert_eq!(
        inl("<https://x.com/a>"),
        vec![Inline::Link(Link {
            url: "https://x.com/a".into(),
            label: None,
            embed: false
        })]
    );
}

#[test]
fn masked_links() {
    assert_eq!(
        inl("[click](https://x.com)"),
        vec![Inline::Link(Link {
            url: "https://x.com".into(),
            label: Some(vec![t("click")]),
            embed: true
        })]
    );
    assert_eq!(
        inl("[**bold** label](https://x.com/(a))"),
        vec![Inline::Link(Link {
            url: "https://x.com/(a)".into(),
            label: Some(vec![b(vec![t("bold")]), t(" label")]),
            embed: true
        })]
    );
    assert_eq!(
        inl("[quiet](<https://x.com>)"),
        vec![Inline::Link(Link {
            url: "https://x.com".into(),
            label: Some(vec![t("quiet")]),
            embed: false
        })]
    );
    // Not links:
    assert_eq!(inl("[a](not a url)"), vec![t("[a](not a url)")]);
    assert_eq!(
        inl("[a] (https://x.com)"),
        vec![t("[a] ("), link("https://x.com"), t(")")]
    );
    assert_eq!(
        inl("[](https://x.com)"),
        vec![t("[]("), link("https://x.com"), t(")")]
    );
    assert_eq!(
        inl("[a](javascript:alert(1))"),
        vec![t("[a](javascript:alert(1))")]
    );
    assert_eq!(
        inl("[a](https://x.com"),
        vec![t("[a]("), link("https://x.com")]
    );
}

#[test]
fn masked_link_inside_bold() {
    assert_eq!(
        inl("**[x](https://a.b)**"),
        vec![b(vec![Inline::Link(Link {
            url: "https://a.b".into(),
            label: Some(vec![t("x")]),
            embed: true
        })])]
    );
}

// ---- discord tokens -----------------------------------------------------------------

#[test]
fn mentions() {
    assert_eq!(inl("<@123>"), vec![Inline::Mention(Mention::User(123))]);
    assert_eq!(inl("<@!123>"), vec![Inline::Mention(Mention::User(123))]);
    assert_eq!(inl("<@&9>"), vec![Inline::Mention(Mention::Role(9))]);
    assert_eq!(inl("<#77>"), vec![Inline::Mention(Mention::Channel(77))]);
    assert_eq!(inl("@everyone"), vec![Inline::Mention(Mention::Everyone)]);
    assert_eq!(
        inl("hi @here!"),
        vec![t("hi "), Inline::Mention(Mention::Here), t("!")]
    );
    assert_eq!(inl("<@abc>"), vec![t("<@abc>")]);
    assert_eq!(inl("<@>"), vec![t("<@>")]);
    assert_eq!(inl("<@123"), vec![t("<@123")]);
    assert_eq!(
        inl("<@99999999999999999999999>"),
        vec![t("<@99999999999999999999999>")]
    );
    assert_eq!(inl("< @123>"), vec![t("< @123>")]);
}

#[test]
fn mention_inside_formatting_and_code() {
    assert_eq!(
        inl("**<@1>**"),
        vec![b(vec![Inline::Mention(Mention::User(1))])]
    );
    assert_eq!(inl("`@everyone`"), vec![code("@everyone")]);
}

#[test]
fn custom_emoji() {
    assert_eq!(
        inl("<:pog_champ:123>"),
        vec![Inline::CustomEmoji {
            name: "pog_champ".into(),
            id: 123,
            animated: false
        }]
    );
    assert_eq!(
        inl("<a:dance:4>"),
        vec![Inline::CustomEmoji {
            name: "dance".into(),
            id: 4,
            animated: true
        }]
    );
    // Underscores in emoji names never start italics.
    assert_eq!(inl("_<:a_b:1>_").len(), 1,);
    assert_eq!(inl("<:bad name:1>"), vec![t("<:bad name:1>")]);
    assert_eq!(inl("<:x:>"), vec![t("<:x:>")]);
    assert_eq!(inl(":shortcode:"), vec![t(":shortcode:")]);
}

#[test]
fn timestamps() {
    assert_eq!(
        inl("<t:1618953630>"),
        vec![Inline::Timestamp {
            unix: 1618953630,
            style: TimestampStyle::ShortDateTime
        }]
    );
    for (c, s) in [
        ('t', TimestampStyle::ShortTime),
        ('T', TimestampStyle::LongTime),
        ('d', TimestampStyle::ShortDate),
        ('D', TimestampStyle::LongDate),
        ('f', TimestampStyle::ShortDateTime),
        ('F', TimestampStyle::LongDateTime),
        ('R', TimestampStyle::Relative),
    ] {
        assert_eq!(
            inl(&format!("<t:1:{c}>")),
            vec![Inline::Timestamp { unix: 1, style: s }]
        );
        assert_eq!(s.as_char(), c);
    }
    assert_eq!(
        inl("<t:-5:R>"),
        vec![Inline::Timestamp {
            unix: -5,
            style: TimestampStyle::Relative
        }]
    );
    assert_eq!(inl("<t:1:x>"), vec![t("<t:1:x>")]);
    assert_eq!(inl("<t:1:RR>"), vec![t("<t:1:RR>")]);
    assert_eq!(inl("<t:abc>"), vec![t("<t:abc>")]);
    assert_eq!(inl("<t:>"), vec![t("<t:>")]);
    assert_eq!(
        inl("<t:99999999999999999999>"),
        vec![t("<t:99999999999999999999>")]
    );
}

#[test]
fn angle_brackets_that_are_not_tokens() {
    assert_eq!(inl("a < b > c"), vec![t("a < b > c")]);
    assert_eq!(
        inl("<<@1>>"),
        vec![t("<"), Inline::Mention(Mention::User(1)), t(">")]
    );
    assert_eq!(inl("<html>"), vec![t("<html>")]);
}

// ---- blocks -------------------------------------------------------------------------

#[test]
fn headings() {
    assert_eq!(
        blocks("# Big"),
        vec![Block::Heading {
            level: 1,
            content: vec![t("Big")]
        }]
    );
    assert_eq!(
        blocks("## *Med*"),
        vec![Block::Heading {
            level: 2,
            content: vec![it(vec![t("Med")])]
        }]
    );
    assert!(matches!(
        blocks("### s")[0],
        Block::Heading { level: 3, .. }
    ));
    assert_eq!(
        blocks("#### four"),
        vec![Block::Paragraph(vec![t("#### four")])]
    );
    assert_eq!(
        blocks("#nospace"),
        vec![Block::Paragraph(vec![t("#nospace")])]
    );
    assert_eq!(blocks("# "), vec![Block::Paragraph(vec![t("# ")])]);
    assert_eq!(
        blocks(" # indented"),
        vec![Block::Paragraph(vec![t(" # indented")])]
    );
}

#[test]
fn heading_between_paragraphs() {
    let v = blocks("intro\n# Title\nbody");
    assert_eq!(v.len(), 3, "{v:?}");
    assert_eq!(v[0], Block::Paragraph(vec![t("intro")]));
    assert!(matches!(v[1], Block::Heading { level: 1, .. }));
    assert_eq!(v[2], Block::Paragraph(vec![t("body")]));
}

#[test]
fn subtext() {
    assert_eq!(blocks("-# tiny"), vec![Block::Subtext(vec![t("tiny")])]);
    assert_eq!(blocks("-#tiny"), vec![Block::Paragraph(vec![t("-#tiny")])]);
}

#[test]
fn quotes() {
    assert_eq!(
        blocks("> a\n> b\nc"),
        vec![
            Block::Quote(vec![Block::Paragraph(vec![t("a\nb")])]),
            Block::Paragraph(vec![t("c")])
        ]
    );
    assert_eq!(
        blocks(">>> all\nof\nthis"),
        vec![Block::Quote(vec![Block::Paragraph(vec![t(
            "all\nof\nthis"
        )])])]
    );
    assert_eq!(
        blocks(">no space"),
        vec![Block::Paragraph(vec![t(">no space")])]
    );
    // No nesting: the inner marker is literal.
    assert_eq!(
        blocks("> > x"),
        vec![Block::Quote(vec![Block::Paragraph(vec![t("> x")])])]
    );
    assert_eq!(
        blocks(">>> > x"),
        vec![Block::Quote(vec![Block::Paragraph(vec![t("> x")])])]
    );
}

#[test]
fn quote_with_blocks_inside() {
    let v = blocks("> # head\n> - item\n> **b**");
    let [Block::Quote(inner)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert!(matches!(inner[0], Block::Heading { .. }));
    assert!(matches!(inner[1], Block::List(_)));
    assert_eq!(inner[2], Block::Paragraph(vec![b(vec![t("b")])]));
}

#[test]
fn quote_after_text() {
    let v = blocks("text\n>>> quoted\nmore");
    assert_eq!(v[0], Block::Paragraph(vec![t("text")]));
    assert_eq!(
        v[1],
        Block::Quote(vec![Block::Paragraph(vec![t("quoted\nmore")])])
    );
}

#[test]
fn lists() {
    let v = blocks("- a\n- b\n* c");
    let [Block::List(l)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert!(!l.ordered);
    assert_eq!(l.items.len(), 3);
    assert_eq!(l.items[2].content, vec![t("c")]);

    let v = blocks("3. x\n4. y");
    let [Block::List(l)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert!(l.ordered);
    assert_eq!(l.start, 3);
    assert_eq!(l.items.len(), 2);
}

#[test]
fn nested_lists() {
    let v = blocks("- a\n  - a1\n  - a2\n- b\n    1. deep");
    let [Block::List(l)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(l.items.len(), 2);
    assert_eq!(l.items[0].children.len(), 1);
    assert_eq!(l.items[0].children[0].items.len(), 2);
    assert!(l.items[1].children[0].ordered);
}

#[test]
fn list_type_switch_starts_new_list() {
    let v = blocks("- a\n1. b");
    assert_eq!(v.len(), 2, "{v:?}");
}

#[test]
fn not_lists() {
    assert_eq!(blocks("-a"), vec![Block::Paragraph(vec![t("-a")])]);
    assert_eq!(blocks("- "), vec![Block::Paragraph(vec![t("- ")])]);
    assert_eq!(blocks("1.no"), vec![Block::Paragraph(vec![t("1.no")])]);
    assert_eq!(
        blocks("1234567890. big"),
        vec![Block::Paragraph(vec![t("1234567890. big")])]
    );
}

#[test]
fn list_items_have_inline_formatting() {
    let v = blocks("- **bold** item");
    let [Block::List(l)] = v.as_slice() else {
        panic!("{v:?}")
    };
    assert_eq!(l.items[0].content, vec![b(vec![t("bold")]), t(" item")]);
}

#[test]
fn deep_list_nesting_is_capped() {
    let src: String = (0..50).map(|i| format!("{}- x\n", " ".repeat(i))).collect();
    let v = blocks(&src);
    assert!(!v.is_empty());
}

// ---- document helpers ---------------------------------------------------------------

#[test]
fn doc_helpers() {
    let d = parse("hi <@5> and `<@6>` @here");
    assert!(d.mentions_user(5));
    assert!(!d.mentions_user(6));
    assert!(d.mentions_everyone());
    assert!(!parse("`@everyone`").mentions_everyone());

    assert!(parse("<:a:1> <a:b:2>").is_emoji_only());
    assert!(!parse("hi <:a:1>").is_emoji_only());
    assert!(!parse("").is_emoji_only());
    assert!(parse("\u{1F980} \u{2764}\u{FE0F}").is_emoji_only());
    assert!(parse("<:a:1> \u{1F44D}\u{1F3FD}").is_emoji_only());
    assert!(!parse("\u{1F980} ok").is_emoji_only());
    assert!(!parse("123 #").is_emoji_only());

    let p = parse("# T\n**b** [l](https://x.y) <@1>").plain_text_with(&|i| match i {
        Inline::Mention(Mention::User(id)) => format!("@u{id}"),
        _ => String::new(),
    });
    assert_eq!(p, "T\nb l @u1");
}

#[test]
fn parse_inline_api() {
    assert_eq!(parse_inline("# not a heading"), vec![t("# not a heading")]);
}

// ---- highlighting -------------------------------------------------------------------

fn kinds(lang: &str, code: &str) -> Vec<(String, HlKind)> {
    highlight(lang, code)
        .into_iter()
        .map(|s| (code[s.start as usize..s.end as usize].to_string(), s.kind))
        .collect()
}

#[test]
fn highlight_rust() {
    let k = kinds("rust", "fn main() { let s = \"hi\"; // c\n 42 }");
    assert!(k.contains(&("fn".into(), HlKind::Keyword)));
    assert!(k.contains(&("let".into(), HlKind::Keyword)));
    assert!(k.contains(&("\"hi\"".into(), HlKind::String)));
    assert!(k.contains(&("// c".into(), HlKind::Comment)));
    assert!(k.contains(&("42".into(), HlKind::Number)));
    let k = kinds("rs", "fn f<'a>(x: &'a str) -> Option<char> { Some('x') }");
    assert!(k.contains(&("'x'".into(), HlKind::String)));
    assert!(k.contains(&("Option".into(), HlKind::Type)));
    assert!(!k.iter().any(|(s, _)| s.starts_with("'a")));
    let k = kinds("rust", "println!(\"x\")");
    assert!(k.contains(&("println!".into(), HlKind::Keyword)));
}

#[test]
fn highlight_others() {
    let k = kinds("py", "def f():\n    return None  # done\n'''doc\nstring'''");
    assert!(k.contains(&("def".into(), HlKind::Keyword)));
    assert!(k.contains(&("None".into(), HlKind::Literal)));
    assert!(k.contains(&("# done".into(), HlKind::Comment)));
    assert!(k.contains(&("'''doc\nstring'''".into(), HlKind::String)));

    let k = kinds("json", "{\"a\": [1, true, null, \"s\"]}");
    assert!(k.contains(&("\"a\"".into(), HlKind::Key)));
    assert!(k.contains(&("\"s\"".into(), HlKind::String)));
    assert!(k.contains(&("true".into(), HlKind::Literal)));

    let k = kinds("toml", "[package]\nname = \"x\" # c\nv = 1.5");
    assert!(k.contains(&("[package]".into(), HlKind::Type)));
    assert!(k.contains(&("name".into(), HlKind::Key)));
    assert!(k.contains(&("1.5".into(), HlKind::Number)));

    let k = kinds("ts", "const x: number = `t${1}`; /* c */");
    assert!(k.contains(&("const".into(), HlKind::Keyword)));
    assert!(k.contains(&("/* c */".into(), HlKind::Comment)));
    assert!(k.iter().any(|(_, k)| *k == HlKind::String));

    assert!(highlight("brainfuck", "+++").is_empty());
}

#[test]
fn highlight_spans_are_char_boundaries() {
    for lang in ["rust", "js", "py", "json", "toml"] {
        let code = "é'ü' \"ñ\\\" 'x\n// ü\n/* é */ 1é ü_x 'ab";
        for s in highlight(lang, code) {
            assert!(s.start < s.end);
            assert!(code.is_char_boundary(s.start as usize));
            assert!(code.is_char_boundary(s.end as usize));
        }
    }
}

// ---- robustness ---------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const PIECES: &[&str] = &[
    "*",
    "**",
    "_",
    "__",
    "~~",
    "||",
    "`",
    "``",
    "```",
    "\\",
    "<",
    ">",
    "<@",
    "<@!",
    "<@&",
    "<#",
    "<:",
    "<a:",
    "<t:",
    ":R>",
    "123",
    "a",
    "b",
    " ",
    "  ",
    "\n",
    "\n\n",
    "# ",
    "## ",
    "-# ",
    "> ",
    ">>> ",
    "- ",
    "* ",
    "1. ",
    "[",
    "]",
    "(",
    ")",
    "](",
    "https://",
    "http://",
    "x.com",
    "@everyone",
    "@here",
    "é",
    "🦀",
    "\u{200d}",
    "rust\n",
    ":",
    "|",
    "~",
    "\t",
    "\r\n",
    "'",
    "\"",
    "/*",
    "//",
    "#",
    "<https://a.b>",
    "[l](https://a.b)",
];

fn random_doc(rng: &mut Rng, max_pieces: usize) -> String {
    let n = rng.below(max_pieces) + 1;
    let mut s = String::new();
    for _ in 0..n {
        if rng.below(10) == 0 {
            // a random char, including unusual ones
            let c = char::from_u32((rng.next() % 0x3000) as u32).unwrap_or('?');
            s.push(c);
        } else {
            s.push_str(PIECES[rng.below(PIECES.len())]);
        }
    }
    s
}

fn check_doc(src: &str, d: &Document) {
    // Every code block's highlight spans index valid char boundaries.
    fn blocks(v: &[Block]) {
        for b in v {
            match b {
                Block::CodeBlock(c) => {
                    for s in &c.highlights {
                        assert!(c.code.is_char_boundary(s.start as usize));
                        assert!(c.code.is_char_boundary(s.end as usize));
                        assert!(s.end as usize <= c.code.len());
                    }
                }
                Block::Quote(i) => {
                    assert!(!i.iter().any(|b| matches!(b, Block::Quote(_))));
                    blocks(i)
                }
                _ => {}
            }
        }
    }
    blocks(&d.blocks);
    // Adjacent text is merged.
    d.walk(&mut |_| {});
    let _ = d.plain_text_with(&|_| String::new());
    let _ = src;
}

#[test]
fn fuzz_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..20_000 {
        let src = random_doc(&mut rng, 40);
        let d = parse(&src);
        check_doc(&src, &d);
        let _ = parse_inline(&src);
    }
}

#[test]
fn fuzz_random_bytes_as_utf8() {
    let mut rng = Rng(42);
    for _ in 0..5_000 {
        let len = rng.below(200);
        let bytes: Vec<u8> = (0..len)
            .map(|_| b"*_~|`<>@#:[]()\\\n -1aht/."[rng.below(24)])
            .collect();
        let src = String::from_utf8_lossy(&bytes);
        let d = parse(&src);
        check_doc(&src, &d);
    }
}

#[test]
fn plain_text_is_preserved_without_markup() {
    // Text without any special characters must come back unchanged.
    let mut rng = Rng(7);
    for _ in 0..2_000 {
        let len = rng.below(80);
        let s: String = (0..len)
            .map(|_| b"abc xyz\n019,."[rng.below(13)] as char)
            .collect();
        let d = parse(&s);
        let back = d.plain_text_with(&|_| String::new());
        let norm: String = s.trim_end_matches('\n').to_string();
        // Paragraph-level trailing newline and leading list-like "1. " can differ;
        // only check strings that cannot form blocks.
        if !s.contains(". ") && !s.ends_with('\n') {
            assert_eq!(back, norm, "{s:?}");
        }
    }
}

fn assert_fast(name: &str, src: &str) {
    let start = std::time::Instant::now();
    let d = parse(src);
    let el = start.elapsed();
    check_doc(src, &d);
    // Generous bound for debug builds on slow CI machines; quadratic behavior on these
    // inputs would take minutes.
    assert!(
        el.as_secs_f32() < 3.0,
        "{name}: {el:?} for {} bytes",
        src.len()
    );
}

#[test]
fn pathological_inputs_are_fast() {
    let n = 40_000;
    assert_fast("stars", &"*".repeat(n));
    assert_fast("star-a", &"*a".repeat(n / 2));
    assert_fast("star-space", &"*a ".repeat(n / 3));
    assert_fast("double-star", &"**a ".repeat(n / 4));
    assert_fast("underscores", &"_".repeat(n));
    assert_fast("under-word", &"_a ".repeat(n / 3));
    assert_fast("tildes", &"~~a".repeat(n / 3));
    assert_fast("pipes", &"||a".repeat(n / 3));
    assert_fast("backticks", &"`".repeat(n));
    assert_fast(
        "tick-runs",
        &(1..300).map(|i| "`".repeat(i) + "x").collect::<String>(),
    );
    assert_fast("angles", &"<".repeat(n));
    assert_fast("angle-at", &"<@".repeat(n / 2));
    assert_fast("brackets", &"[".repeat(n));
    assert_fast("bracket-close", &"[a](".repeat(n / 4));
    assert_fast("fences", &"```".repeat(n / 3));
    assert_fast("fence-lines", &"```\n".repeat(n / 4));
    assert_fast("quotes", &"> a\n".repeat(n / 4));
    assert_fast("lists", &"- a\n".repeat(n / 4));
    assert_fast("urls", &"https://a.b/".repeat(n / 12));
    assert_fast("url-parens", &format!("https://a.b/{}", ")".repeat(n)));
    assert_fast("escapes", &"\\*".repeat(n / 2));
    assert_fast("mixed", &"**_~~||`<@1>".repeat(n / 12));
    assert_fast("nested-open", &"*_**__~~||".repeat(n / 10));
    assert_fast("headings", &"# a\n".repeat(n / 4));
}

#[test]
fn deep_nesting_does_not_overflow() {
    let n = 5_000;
    let src = format!("{}x{}", "**_".repeat(n), "_**".repeat(n));
    let _ = parse(&src);
    let src = format!("{}x{}", "||".repeat(n), "||".repeat(n));
    let _ = parse(&src);
    let src = format!("{}x{}", "[".repeat(n), "](https://a.b)".repeat(n));
    let _ = parse(&src);
}

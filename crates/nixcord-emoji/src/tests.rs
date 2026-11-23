use super::*;

#[test]
fn twemoji_file_names() {
    // ❤️: no ZWJ, so U+FE0F is dropped.
    assert_eq!(twemoji_name("\u{2764}\u{FE0F}"), "2764");
    // 👍🏽: base + skin tone.
    assert_eq!(twemoji_name("\u{1F44D}\u{1F3FD}"), "1f44d-1f3fd");
    // 🏳️‍🌈: contains a ZWJ, so U+FE0F stays.
    assert_eq!(
        twemoji_name("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"),
        "1f3f3-fe0f-200d-1f308"
    );
    // 1️⃣: keycaps lose their U+FE0F.
    assert_eq!(twemoji_name("1\u{FE0F}\u{20E3}"), "31-20e3");
    // 🇯🇵: regional indicator pair.
    assert_eq!(twemoji_name("\u{1F1EF}\u{1F1F5}"), "1f1ef-1f1f5");
    // 👨‍👩‍👧: ZWJ family.
    assert_eq!(
        twemoji_name("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        "1f468-200d-1f469-200d-1f467"
    );
    // ©️: no leading zeros.
    assert_eq!(twemoji_name("\u{00A9}\u{FE0F}"), "a9");
    // The one asset Twemoji names unqualified.
    assert_eq!(
        twemoji_name("\u{1F441}\u{FE0F}\u{200D}\u{1F5E8}\u{FE0F}"),
        "1f441-200d-1f5e8"
    );
    assert_eq!(
        twemoji_url("\u{1F980}"),
        "https://cdn.jsdelivr.net/gh/jdecked/twemoji@v17.0.3/assets/72x72/1f980.png"
    );
}

#[test]
fn table_loads_and_is_ordered() {
    assert_eq!(len(), data::COUNT);
    assert!(len() > 1800);
    let mut prev = Category::People;
    for e in all() {
        assert!(e.category() >= prev, "{e:?} out of order");
        prev = e.category();
        assert!(!e.shortcode().is_empty(), "{e:?} has no shortcode");
        assert_eq!(find(e.as_str()), Some(e), "{e:?} not found by itself");
    }
    for c in Category::ALL {
        assert!(category(c).len() > 20, "{c:?}");
        assert!(category(c).all(|e| e.category() == c));
        assert!(find(c.icon()).is_some(), "{c:?} icon");
    }
    let total: usize = Category::ALL.iter().map(|&c| category(c).len()).sum();
    assert_eq!(total, len());
}

#[test]
fn shortcodes() {
    let (heart, tone) = by_shortcode("heart").unwrap();
    assert_eq!((heart.as_str(), tone), ("\u{2764}\u{FE0F}", 0));
    assert!(heart.shortcodes().any(|n| n == "red_heart"));
    assert_eq!(by_shortcode(":Heart:").unwrap().0, heart);
    assert_eq!(by_shortcode("thumbsup").unwrap().0.as_str(), "\u{1F44D}");
    assert_eq!(by_shortcode("+1").unwrap().0.as_str(), "\u{1F44D}");
    assert_eq!(by_shortcode("crab").unwrap().0.as_str(), "\u{1F980}");
    assert_eq!(
        by_shortcode("flag_jp").unwrap().0.as_str(),
        "\u{1F1EF}\u{1F1F5}"
    );
    assert_eq!(by_shortcode("joy").unwrap().0.as_str(), "\u{1F602}");
    let (e, tone) = by_shortcode("thumbsup_tone3").unwrap();
    assert_eq!(e.with_skin_tone(tone), "\u{1F44D}\u{1F3FD}");
    assert!(by_shortcode("heart_tone3").is_none()); // no skin tones
    assert!(by_shortcode("not_an_emoji_name").is_none());
    assert!(by_shortcode("").is_none());
}

#[test]
fn skin_tones() {
    let s = |n: &str, t: u8| by_shortcode(n).unwrap().0.with_skin_tone(t).into_owned();
    assert_eq!(s("thumbsup", 0), "\u{1F44D}");
    assert_eq!(s("thumbsup", 5), "\u{1F44D}\u{1F3FF}");
    // ZWJ sequence: the modifier goes after the person.
    assert_eq!(
        s("woman_technologist", 2),
        "\u{1F469}\u{1F3FC}\u{200D}\u{1F4BB}"
    );
    // U+FE0F after the base is replaced by the modifier.
    assert_eq!(s("detective", 1), "\u{1F575}\u{1F3FB}");
    // Two people both take the tone.
    assert_eq!(
        s("people_holding_hands", 3),
        "\u{1F9D1}\u{1F3FD}\u{200D}\u{1F91D}\u{200D}\u{1F9D1}\u{1F3FD}"
    );
    assert_eq!(s("heart", 3), "\u{2764}\u{FE0F}");
}

#[test]
fn finds_variants() {
    let thumbs = find("\u{1F44D}").unwrap();
    assert_eq!(find("\u{1F44D}\u{1F3FD}"), Some(thumbs));
    assert_eq!(find("\u{2764}"), find("\u{2764}\u{FE0F}"));
    assert!(find("a").is_none());
    assert!(find("").is_none());
}

#[test]
fn segmentation() {
    use TextOrEmoji::*;
    fn runs(s: &str) -> Vec<TextOrEmoji<'_>> {
        emoji_runs(s).collect()
    }
    assert_eq!(runs(""), []);
    assert_eq!(runs("plain #1 *ok* 2024"), [Text("plain #1 *ok* 2024")]);
    assert_eq!(
        runs("caf\u{e9} \u{a9} \u{2122}"),
        [Text("caf\u{e9} \u{a9} \u{2122}")]
    );
    assert_eq!(
        runs("a\u{1F980}b"),
        [Text("a"), Emoji("\u{1F980}"), Text("b")]
    );
    let samples = [
        "\u{2764}\u{FE0F}",
        "\u{1F44D}\u{1F3FD}",
        "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
        "1\u{FE0F}\u{20E3}",
        "\u{1F1EF}\u{1F1F5}",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{00A9}\u{FE0F}",
        "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
    ];
    for s in samples {
        assert_eq!(runs(s), [Emoji(s)], "{s:?}");
        assert!(is_emoji(s), "{s:?}");
    }
    let joined = samples.join("");
    assert_eq!(runs(&joined).len(), samples.len());
    assert_eq!(
        runs("x \u{2764}\u{FE0F}\u{2764}\u{FE0F} y"),
        [
            Text("x "),
            Emoji("\u{2764}\u{FE0F}"),
            Emoji("\u{2764}\u{FE0F}"),
            Text(" y")
        ]
    );
    // Text-presentation characters without U+FE0F stay text; digits and # stay text.
    for s in [
        "\u{2764}",
        "\u{a9}",
        "1",
        "#",
        "*",
        "1\u{FE0F}",
        "\u{1F1EF}",
    ] {
        assert!(!is_emoji(s), "{s:?}");
    }
    assert_eq!(count_emoji_only(" \u{1F980} \u{1F44D}\u{1F3FD} "), Some(2));
    assert_eq!(count_emoji_only("hi \u{1F980}"), None);
    assert_eq!(count_emoji_only(""), Some(0));
}

#[test]
fn search_ranking() {
    let names = |q: &str| -> Vec<&str> {
        search(q, 8)
            .into_iter()
            .map(|(e, _)| e.shortcode())
            .collect()
    };
    // Exact first.
    let r = search("heart", 20);
    assert_eq!(r[0].0.shortcode(), "heart");
    assert_eq!(r[0].1, MatchKind::Exact);
    // Then prefixes before word / keyword matches.
    let kinds: Vec<MatchKind> = r.iter().map(|x| x.1).collect();
    let mut sorted = kinds.clone();
    sorted.sort();
    assert_eq!(kinds, sorted);
    assert!(names("hea").contains(&"heart"));
    assert_eq!(search("thumbsup", 1)[0].0.as_str(), "\u{1F44D}");
    // Keyword-only match ("lol" is a tag of 😄).
    assert!(search("lol", 50)
        .iter()
        .any(|(e, k)| e.as_str() == "\u{1F604}" && *k == MatchKind::Keyword));
    assert!(search("", 10).is_empty());
    assert!(search("zzzzqqq", 10).is_empty());
    assert_eq!(search("smile", 3).len(), 3);
}

#[test]
fn autocomplete_query() {
    assert_eq!(shortcode_query("hello :hea"), Some((6, "hea")));
    assert_eq!(shortcode_query(":he"), Some((0, "he")));
    assert_eq!(shortcode_query(":h"), None);
    assert_eq!(shortcode_query("at 10:30"), None);
    assert_eq!(shortcode_query("<:ferr"), None);
    assert_eq!(shortcode_query(":heart: x"), None);
    assert_eq!(shortcode_query("(:thumbs"), Some((1, "thumbs")));
}

#[test]
fn shortcode_replacement() {
    let none = |_: &str| None;
    assert_eq!(
        replace_shortcodes("i :heart: it", none),
        "i \u{2764}\u{FE0F} it"
    );
    assert_eq!(
        replace_shortcodes(":thumbsup_tone2::crab:", none),
        "\u{1F44D}\u{1F3FC}\u{1F980}"
    );
    assert_eq!(replace_shortcodes("at 10:30:45", none), "at 10:30:45");
    assert_eq!(
        replace_shortcodes("`:heart:` :heart:", none),
        "`:heart:` \u{2764}\u{FE0F}"
    );
    assert_eq!(
        replace_shortcodes("```\n:heart:\n```", none),
        "```\n:heart:\n```"
    );
    assert_eq!(
        replace_shortcodes("<:heart:123> <a:crab:4>", none),
        "<:heart:123> <a:crab:4>"
    );
    assert_eq!(
        replace_shortcodes(":nope:heart:", none),
        ":nope\u{2764}\u{FE0F}"
    );
    let custom = |n: &str| (n == "ferris").then(|| "<:ferris:42>".to_string());
    assert_eq!(replace_shortcodes("hi :ferris:", custom), "hi <:ferris:42>");
    assert!(matches!(
        replace_shortcodes("no colons", none),
        Cow::Borrowed(_)
    ));
}

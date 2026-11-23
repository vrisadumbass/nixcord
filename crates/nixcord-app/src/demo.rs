//! `--demo` backend: fake data fed through the exact same `AppEvent` path as the real
//! backend, with no network access. Used for screenshots and RAM measurements.
//!
//! The data deliberately exercises everything the model can carry: member roles and role
//! colors, nicknames, reactions (some by me), rich and link embeds, replies, mentions of
//! me (direct and via a role), unicode and custom emoji, and markdown-heavy content.

use crate::backend::{AppEvent, Command, EventTx, HISTORY_PAGE};
use nixcord_model::{
    Attachment, Channel, ChannelKind, Embed, EmbedAuthor, EmbedField, EmbedFooter, EmbedMedia,
    Emoji, GatewayEvent, Guild, Message, MessageReference, MessageUpdate, PartialMember, Reaction,
    Ready, Role, Snowflake, User,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
const ME: u64 = 1000;
const BOT: u64 = 1001;

/// Role ids are `guild * 1000 + k`; `@everyone` uses the guild id, as on Discord.
const ROLE_HELPER: u64 = 1;
const ROLE_DEV: u64 = 2;
const ROLE_ARTIST: u64 = 3;
const ROLE_MOD: u64 = 4;

/// Fake custom emoji ids (rendering them needs the CDN; the demo backend never fetches).
const EMOJI_FERRIS: u64 = 1_150_000_000_000_000_001;
const EMOJI_PARTY: u64 = 1_150_000_000_000_000_002;

/// The channel that carries the feature showcase (Hearth #general).
const SHOWCASE_CHANNEL: u64 = 102;
/// First history load of this channel fails, to exercise the per-channel Retry.
const FLAKY_CHANNEL: u64 = 107;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(DISCORD_EPOCH_MS, |d| d.as_millis() as u64)
}

fn snowflake_at(ms: u64, seq: u64) -> Snowflake {
    Snowflake((ms.saturating_sub(DISCORD_EPOCH_MS) << 22) | (seq & 0x3f_ffff))
}

fn iso(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

fn user(id: u64, username: &str, global: Option<&str>) -> User {
    User {
        id: Snowflake(id),
        username: username.into(),
        global_name: global.map(Into::into),
        avatar: None,
        bot: id == BOT,
    }
}

fn people() -> Vec<User> {
    vec![
        user(ME, "you", Some("You")),
        user(BOT, "hearthbot", Some("hearthbot")),
        user(1002, "sol", Some("Sol")),
        user(1003, "quill", Some("Quill")),
        user(1004, "pip", None),
        user(1005, "ferris", Some("Ferris")),
        user(1006, "mira_dev", Some("Mira")),
        user(1007, "tamsin", Some("Tamsin")),
        user(1008, "otto", Some("Otto Kranz")),
    ]
}

/// Guild the demo channel belongs to (guild channels are numbered `guild * 10 + n`).
fn guild_of(channel: Snowflake) -> Option<Snowflake> {
    (channel.0 >= 100).then(|| Snowflake(channel.0 / 100 * 10))
}

fn role_id(guild: Snowflake, k: u64) -> Snowflake {
    Snowflake(guild.0 * 1000 + k)
}

/// Deterministic member data per user: role set and an occasional nickname.
fn member_for(user_id: Snowflake, guild: Snowflake) -> PartialMember {
    let roles: &[u64] = match user_id.0 {
        ME => &[ROLE_DEV],
        BOT => &[ROLE_HELPER],
        1002 => &[ROLE_MOD, ROLE_HELPER],
        1003 => &[ROLE_DEV],
        1005 => &[ROLE_ARTIST, ROLE_HELPER],
        1006 => &[ROLE_DEV, ROLE_MOD],
        1007 => &[ROLE_ARTIST],
        1008 => &[ROLE_HELPER],
        _ => &[],
    };
    let nick = match user_id.0 {
        1005 => Some("Ferris the Crab"),
        1006 => Some("mira (she/her)"),
        _ => None,
    };
    PartialMember {
        roles: roles.iter().map(|&k| role_id(guild, k)).collect(),
        nick: nick.map(Into::into),
        avatar: None,
    }
}

fn chan(
    id: u64,
    guild: Option<u64>,
    kind: ChannelKind,
    name: &str,
    parent: Option<u64>,
    position: i32,
    topic: Option<&str>,
) -> Channel {
    Channel {
        id: Snowflake(id),
        kind,
        guild_id: guild.map(Snowflake),
        name: Some(name.into()),
        topic: topic.map(Into::into),
        position,
        parent_id: parent.map(Snowflake),
        recipients: Vec::new(),
        last_message_id: None,
    }
}

fn guild(id: u64, name: &str, channels: Vec<Channel>) -> Guild {
    let g = Snowflake(id);
    let role = |k: u64, name: &str, color: u32, position: i32| Role {
        id: role_id(g, k),
        name: name.into(),
        color,
        position,
    };
    Guild {
        id: g,
        name: name.into(),
        icon: None,
        channels,
        roles: vec![
            Role {
                id: g,
                name: "@everyone".into(),
                color: 0,
                position: 0,
            },
            role(ROLE_HELPER, "helper", 0x2ECC71, 1),
            role(ROLE_DEV, "dev", 0xF1C40F, 2),
            role(ROLE_ARTIST, "artist", 0x9B59B6, 3),
            role(ROLE_MOD, "mod", 0xE74C3C, 4),
        ],
        unavailable: false,
    }
}

fn ready() -> Ready {
    use ChannelKind::*;
    let t = Some;
    let nix = guild(
        10,
        "Hearth",
        vec![
            chan(
                100,
                t(10),
                Text,
                "changelog",
                None,
                0,
                Some("Release notes and announcements"),
            ),
            chan(101, t(10), Category, "chat", None, 1, None),
            chan(
                102,
                t(10),
                Text,
                "general",
                t(101),
                0,
                Some("General chat about the bot. Be nice."),
            ),
            chan(
                103,
                t(10),
                Text,
                "suggestions",
                t(101),
                1,
                Some("Ideas welcome"),
            ),
            chan(
                104,
                t(10),
                Text,
                "bugs",
                t(101),
                2,
                Some("Please include your version and logs"),
            ),
            chan(105, t(10), Category, "staff", None, 2, None),
            chan(106, t(10), Text, "release-notes", t(105), 0, None),
            chan(107, t(10), Text, "logs", t(105), 1, None),
            chan(108, t(10), Voice, "Lounge", t(105), 2, None),
        ],
    );
    let rust = guild(
        20,
        "Rust Lounge",
        vec![
            chan(200, t(20), Category, "Text", None, 0, None),
            chan(
                201,
                t(20),
                Text,
                "general",
                t(200),
                0,
                Some("Talk about anything Rust"),
            ),
            chan(
                202,
                t(20),
                Text,
                "egui",
                t(200),
                1,
                Some("Immediate-mode GUI fans"),
            ),
            chan(203, t(20), Announcement, "announcements", t(200), 2, None),
            chan(204, t(20), Forum, "help-forum", t(200), 3, None),
            chan(205, t(20), Category, "Voice", None, 1, None),
            chan(206, t(20), Voice, "Pairing", t(205), 0, None),
        ],
    );
    let pixel = guild(
        30,
        "Pixel Art Club",
        vec![
            chan(
                300,
                t(30),
                Text,
                "showcase",
                None,
                0,
                Some("Share your work"),
            ),
            chan(301, t(30), Text, "feedback", None, 1, None),
        ],
    );
    let synth = guild(
        40,
        "synthwave",
        vec![chan(
            400,
            t(40),
            Text,
            "tracks",
            None,
            0,
            Some("Neon nights only"),
        )],
    );
    let coffee = guild(
        50,
        "Coffee Break",
        vec![chan(
            500,
            t(50),
            Text,
            "brew-log",
            None,
            0,
            Some("V60 vs Aeropress, round 47"),
        )],
    );

    let p = people();
    let dm = |id: u64, who: &[usize]| Channel {
        id: Snowflake(id),
        kind: if who.len() > 1 { GroupDm } else { Dm },
        guild_id: None,
        name: None,
        topic: None,
        position: 0,
        parent_id: None,
        recipients: who.iter().filter_map(|&i| p.get(i).cloned()).collect(),
        last_message_id: Some(snowflake_at(now_ms() - id * 60_000, 0)),
    };

    Ready {
        user: p[0].clone(),
        guilds: vec![nix, rust, pixel, synth, coffee],
        private_channels: vec![
            dm(1, &[2]),
            dm(2, &[3]),
            dm(3, &[5]),
            dm(4, &[4, 6, 7]),
            dm(5, &[8]),
            dm(6, &[1]),
        ],
        session_id: "demo".into(),
        resume_gateway_url: None,
    }
}

const LINES: &[&str] = &[
    "Does the client remember which channel I had open after a restart?",
    "Pretty sure it does, the last channel is saved with the theme.",
    "Does the client remember which channel I had open after a restart?",
    "would be a one-line change in the prefs struct honestly",
    "For some reason my scroll position jumps when an image loads. Am I doing something wrong?",
    "check settings -> chat, there is a toggle for that.",
    "also make sure you are on the latest build, that got fixed last week",
    "lol",
    "ok that fixed it, thanks!",
    "Anyone tried the new build? Startup feels instant now.",
    "the new theme picker is in the settings window",
    "nice, the gradient themes look great btw",
    "I'll look into it when I get home",
    "can we get a compact mode at some point?",
    "https://github.com/emilk/egui is what the UI is built on",
    "pushed a fix for the reconnect loop, please retest",
    "brb coffee",
    "works on my machine (tm)",
    "here is the log from last night",
    "Is there a way to change the accent colour? Asking for a friend who likes purple.",
];

fn unicode(name: &str) -> Emoji {
    Emoji {
        id: None,
        name: Some(name.into()),
        animated: false,
    }
}

fn custom_emoji(id: u64, name: &str, animated: bool) -> Emoji {
    Emoji {
        id: Some(Snowflake(id)),
        name: Some(name.into()),
        animated,
    }
}

fn reaction(emoji: Emoji, count: u32, me: bool) -> Reaction {
    Reaction { emoji, count, me }
}

/// A plain message skeleton; callers fill in the extras.
fn base_msg(id: Snowflake, channel: Snowflake, author: &User, content: String, t: u64) -> Message {
    let guild = guild_of(channel);
    Message {
        id,
        channel_id: channel,
        guild_id: guild,
        member: guild.map(|g| member_for(author.id, g)),
        author: author.clone(),
        content,
        timestamp: iso(t),
        ..Default::default()
    }
}

fn reference_to(m: &Message) -> MessageReference {
    MessageReference {
        message_id: Some(m.id),
        channel_id: Some(m.channel_id),
        guild_id: m.guild_id,
    }
}

/// Make `m` a reply to `target` (reference + a shallow copy of the target).
fn reply_to(m: &mut Message, target: &Message) {
    let mut quoted = target.clone();
    quoted.referenced_message = None;
    m.message_reference = Some(reference_to(target));
    m.referenced_message = Some(Box::new(quoted));
}

fn history_for(channel: Snowflake, people: &[User]) -> Vec<Message> {
    // Deterministic per channel: 120 messages spread over the last ~2 days, ending a
    // quarter of an hour ago (the showcase, if any, comes after).
    let n = 120u64;
    let now = now_ms();
    let end = now - 15 * 60 * 1000;
    let span = 40 * 3600 * 1000u64;
    let mut seed = channel.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let showcase = channel.0 == SHOWCASE_CHANNEL;
    let mut out: Vec<Message> = Vec::with_capacity(n as usize + 8);
    let mut t = end - span;
    let mut author = 2usize;
    for i in 0..n {
        // Bursts: mostly short gaps, sometimes a long pause (so grouping and day
        // dividers both show up).
        let r = next();
        t += match r % 20 {
            0 => 5 * 3600 * 1000,
            1..=4 => 12 * 60 * 1000,
            _ => 30 * 1000 + (r % 60_000),
        };
        let t = t.min(end - (n - i) * 1000);
        if r % 3 == 0 {
            author = 2 + (next() as usize % (people.len() - 2));
        }
        let a = if r % 11 == 0 { 0 } else { author };
        let content = LINES[(next() as usize) % LINES.len()].to_string();
        let mut m = base_msg(snowflake_at(t, i), channel, &people[a], content, t);
        m.edited_timestamp = (r % 13 == 0).then(|| iso(t + 30_000));
        if m.content.starts_with("here is the log") {
            m.attachments.push(Attachment {
                id: snowflake_at(t, i + 1),
                filename: "hearthbot-2026-11-20.log".into(),
                url: "https://example.com/hearthbot.log".into(),
                size: 48_213,
                content_type: Some("text/plain".into()),
                width: None,
                height: None,
            });
        }
        match r % 9 {
            0 => m.reactions.push(reaction(unicode("\u{1F602}"), 2, false)),
            1 => {
                m.reactions.push(reaction(unicode("\u{1F44D}"), 3, true));
                m.reactions.push(reaction(
                    custom_emoji(EMOJI_FERRIS, "ferris", false),
                    1,
                    false,
                ));
            }
            _ => {}
        }
        if r % 7 == 0 {
            if let Some(prev) = out.iter().rev().nth(3).cloned() {
                reply_to(&mut m, &prev);
            }
        }
        out.push(m);
    }
    if showcase {
        out.extend(showcase_messages(channel, people, &out, now));
    }
    out
}

/// One message per feature, newest in the channel so it is on screen when opened.
fn showcase_messages(
    channel: Snowflake,
    people: &[User],
    earlier: &[Message],
    now: u64,
) -> Vec<Message> {
    let guild = guild_of(channel).unwrap_or(Snowflake(10));
    let (bot, juan, rebrax, ferris, mira) =
        (&people[1], &people[2], &people[3], &people[5], &people[6]);
    let mut seq = 0u64;
    let mut at = |minutes_ago: u64| {
        seq += 1;
        let t = now - minutes_ago * 60 * 1000;
        (snowflake_at(t, 0x3f_0000 + seq), t)
    };
    let mut out = Vec::new();

    // Markdown-heavy announcement with user mentions.
    let (id, t) = at(12);
    let mut md = base_msg(
        id,
        channel,
        mira,
        format!(
            "**nixcord 0.2** is *almost* ready. Try `cargo run --release -- --demo`.\n\
             > replies, reactions, embeds and role colors all landed this week\n\
             The secret: ||it idles well under 80 MB||. Notes are in \
             [the architecture doc](https://github.com/vrisadumbass/nixcord) \u{2014} \
             thanks <@{}> and <@{}>!",
            juan.id, rebrax.id
        ),
        t,
    );
    md.mentions = vec![juan.clone(), rebrax.clone()];
    md.reactions = vec![
        reaction(unicode("\u{1F389}"), 6, true),
        reaction(unicode("\u{2764}\u{FE0F}"), 3, false),
        reaction(custom_emoji(EMOJI_PARTY, "party", true), 2, true),
    ];
    out.push(md);

    // Code block.
    let (id, t) = at(11);
    out.push(base_msg(
        id,
        channel,
        ferris,
        "the whole store fold is one match:\n```rust\nfn apply(&mut self, ev: &GatewayEvent) {\n    match ev {\n        GatewayEvent::MessageCreate(m) => self.message_create(m),\n        GatewayEvent::ReactionAdd { .. } => { /* count += 1 */ }\n        _ => {}\n    }\n}\n```\nno locks, no async, just `&mut self`".into(),
        t,
    ));

    // Rich embed from the bot.
    let (id, t) = at(10);
    let mut ci = base_msg(id, channel, bot, String::new(), t);
    ci.embeds.push(Embed {
        kind: Some("rich".into()),
        title: Some("Build #482 passed".into()),
        description: Some("All **63** tests green on `windows-latest` and `ubuntu-latest`.".into()),
        url: Some("https://github.com/vrisadumbass/nixcord/actions".into()),
        color: Some(0x2ECC71),
        author: Some(EmbedAuthor {
            name: "hearthbot CI".into(),
            url: None,
            icon_url: None,
        }),
        footer: Some(EmbedFooter {
            text: "nixcord CI \u{2022} main".into(),
            icon_url: None,
        }),
        fields: vec![
            EmbedField {
                name: "Duration".into(),
                value: "3m 12s".into(),
                inline: true,
            },
            EmbedField {
                name: "Binary".into(),
                value: "7.9 MB".into(),
                inline: true,
            },
            EmbedField {
                name: "Commit".into(),
                value: "`a1b2c3d`".into(),
                inline: true,
            },
            EmbedField {
                name: "Changes".into(),
                value: "reactions, replies, embeds, sliding history window".into(),
                inline: false,
            },
        ],
        ..Default::default()
    });
    ci.reactions = vec![reaction(unicode("\u{1F680}"), 4, false)];
    out.push(ci);

    // Link embed with a provider.
    let (id, t) = at(9);
    let mut link = base_msg(
        id,
        channel,
        juan,
        "this is what the UI is built on https://github.com/emilk/egui".into(),
        t,
    );
    link.embeds.push(Embed {
        kind: Some("article".into()),
        title: Some("emilk/egui: an easy-to-use immediate mode GUI in Rust".into()),
        description: Some(
            "egui runs on the web, natively, and in your favorite game engine.".into(),
        ),
        url: Some("https://github.com/emilk/egui".into()),
        color: Some(0x1E2327),
        provider: Some(EmbedAuthor {
            name: "GitHub".into(),
            url: Some("https://github.com".into()),
            icon_url: None,
        }),
        image: Some(EmbedMedia {
            url: "https://opengraph.githubassets.com/demo/emilk/egui".into(),
            proxy_url: None,
            width: Some(1200),
            height: Some(600),
        }),
        ..Default::default()
    });
    out.push(link);

    // Reply that mentions me.
    let (id, t) = at(8);
    let mut reply = base_msg(
        id,
        channel,
        rebrax,
        format!("<@{ME}> did you see this? the reply preview works now \u{1F440}"),
        t,
    );
    if let Some(target) = out.first().or(earlier.last()) {
        let target = target.clone();
        reply_to(&mut reply, &target);
    }
    reply.mentions = vec![people[0].clone()];
    out.push(reply);

    // Unicode and custom emoji in content.
    let (id, t) = at(7);
    let mut emoji = base_msg(
        id,
        channel,
        ferris,
        format!(
            "unicode \u{1F980}\u{1F680}\u{2728} and custom <:ferris:{EMOJI_FERRIS}> \
             <a:party:{EMOJI_PARTY}> in one line"
        ),
        t,
    );
    emoji.reactions = vec![reaction(
        custom_emoji(EMOJI_FERRIS, "ferris", false),
        5,
        true,
    )];
    out.push(emoji);

    // Role mention: I have @dev, so this pings me too.
    let (id, t) = at(6);
    let dev = role_id(guild, ROLE_DEV);
    let mut standup = base_msg(
        id,
        channel,
        juan,
        format!("<@&{dev}> standup in 10 minutes, bring your *RAM numbers*"),
        t,
    );
    standup.mention_roles = vec![dev];
    out.push(standup);

    out
}

pub async fn run(mut commands: mpsc::UnboundedReceiver<Command>, events: EventTx) {
    let people = people();
    let mut histories: HashMap<Snowflake, Vec<Message>> = HashMap::new();
    let mut failed_once: HashSet<Snowflake> = HashSet::new();
    let mut seq: u64 = 0;

    events.send(AppEvent::LoginOk(people[0].clone()));
    events.send(AppEvent::Gateway(GatewayEvent::Connecting));
    tokio::time::sleep(Duration::from_millis(150)).await;
    events.send(AppEvent::Gateway(GatewayEvent::Ready(Box::new(ready()))));

    // A little later, activity elsewhere: shows unread channels, DMs and servers.
    {
        let events = events.clone();
        let people = people.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let posts: [(u64, usize, &str); 3] = [
                (
                    104,
                    3,
                    "found a crash when the window is minimized, logs incoming",
                ),
                (201, 5, "anyone tried the new borrow checker diagnostics?"),
                (2, 3, "you around? got a minute"),
            ];
            for (i, (channel, who, text)) in posts.into_iter().enumerate() {
                let t = now_ms();
                let m = base_msg(
                    snowflake_at(t, 0x3f_0000 + i as u64),
                    Snowflake(channel),
                    &people[who.min(people.len() - 1)],
                    text.into(),
                    t,
                );
                events.send(AppEvent::Gateway(GatewayEvent::MessageCreate(Box::new(m))));
            }
        });
    }

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Login(_) => {
                events.send(AppEvent::LoginOk(people[0].clone()));
                events.send(AppEvent::Gateway(GatewayEvent::Ready(Box::new(ready()))));
            }
            Command::Logout => {}
            Command::LoadHistory { channel, before } => {
                let events = events.clone();
                if channel.0 == FLAKY_CHANNEL && failed_once.insert(channel) {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        events.send(AppEvent::HistoryFailed {
                            channel,
                            error: "Couldn't load messages: demo network hiccup".into(),
                        });
                    });
                    continue;
                }
                let all = histories
                    .entry(channel)
                    .or_insert_with(|| history_for(channel, &people));
                let page: Vec<Message> = all
                    .iter()
                    .filter(|m| before.is_none_or(|b| m.id < b))
                    .rev()
                    .take(HISTORY_PAGE as usize)
                    .cloned()
                    .collect();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    events.send(AppEvent::History {
                        channel,
                        before,
                        messages: page,
                    });
                });
            }
            Command::Send {
                channel,
                content,
                reply_to: reply_target,
            } => {
                seq += 1;
                let t = now_ms();
                let mut mine = base_msg(snowflake_at(t, seq), channel, &people[0], content, t);
                if let Some(target) = reply_target.and_then(|id| {
                    histories
                        .get(&channel)
                        .and_then(|h| h.iter().find(|m| m.id == id))
                }) {
                    reply_to(&mut mine, target);
                    if target.author.id != mine.author.id {
                        mine.mentions.push(target.author.clone());
                    }
                }
                histories.entry(channel).or_default().push(mine.clone());
                events.send(AppEvent::Sent(Box::new(mine.clone())));
                if mine.content.trim_start().starts_with("/forbidden") {
                    events.send(AppEvent::Forbidden { channel });
                    continue;
                }
                // The demo bot "types" and then echoes the message back as a reply.
                let events = events.clone();
                let bot = people[1].clone();
                seq += 1;
                let t = now_ms() + 2900;
                let mut echo = base_msg(
                    snowflake_at(t, seq),
                    channel,
                    &bot,
                    format!("echo: {}", mine.content),
                    t,
                );
                echo.mentions = vec![mine.author.clone()];
                reply_to(&mut echo, &mine);
                histories.entry(channel).or_default().push(echo.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    events.send(AppEvent::Gateway(GatewayEvent::TypingStart {
                        channel_id: channel,
                        user_id: bot.id,
                    }));
                    tokio::time::sleep(Duration::from_millis(2200)).await;
                    events.send(AppEvent::Gateway(GatewayEvent::MessageCreate(Box::new(
                        echo,
                    ))));
                });
            }
            Command::Edit {
                channel,
                message,
                content,
            } => {
                events.send(AppEvent::Gateway(GatewayEvent::MessageUpdate(Box::new(
                    MessageUpdate {
                        id: message,
                        channel_id: channel,
                        content: Some(content),
                        edited_timestamp: Some(iso(now_ms())),
                        ..Default::default()
                    },
                ))));
            }
            Command::Delete { channel, message } => {
                if let Some(h) = histories.get_mut(&channel) {
                    h.retain(|m| m.id != message);
                }
                events.send(AppEvent::Gateway(GatewayEvent::MessageDelete {
                    id: message,
                    channel_id: channel,
                }));
            }
            Command::React {
                channel,
                message,
                emoji,
            } => {
                events.send(AppEvent::Gateway(GatewayEvent::ReactionAdd {
                    channel_id: channel,
                    message_id: message,
                    user_id: Snowflake(ME),
                    emoji,
                }));
            }
            Command::Unreact {
                channel,
                message,
                emoji,
            } => {
                events.send(AppEvent::Gateway(GatewayEvent::ReactionRemove {
                    channel_id: channel,
                    message_id: message,
                    user_id: Snowflake(ME),
                    emoji,
                }));
            }
            Command::Typing { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn showcase_covers_features() {
        let people = people();
        let ch = Snowflake(SHOWCASE_CHANNEL);
        let h = history_for(ch, &people);
        assert!(
            h.windows(2).all(|w| w[0].id < w[1].id),
            "sorted, unique ids"
        );
        assert!(h.iter().all(|m| m.member.is_some() && m.guild_id.is_some()));
        assert!(h.iter().any(|m| m.reactions.iter().any(|r| r.me)));
        assert!(h.iter().any(|m| m
            .embeds
            .iter()
            .any(|e| !e.fields.is_empty() && e.footer.is_some() && e.color.is_some())));
        assert!(h
            .iter()
            .any(|m| m.embeds.iter().any(|e| e.provider.is_some())));
        assert!(h.iter().any(|m| m.message_reference.is_some()));
        assert!(h.iter().any(|m| m.mentions.iter().any(|u| u.id.0 == ME)));
        assert!(h.iter().any(|m| !m.mention_roles.is_empty()));
        assert!(h.iter().any(|m| m.content.contains("<:ferris:")));
        assert!(h
            .iter()
            .any(|m| m.content.contains("```rust") || m.content.contains("||")));

        // Role colors resolve through the real store.
        let mut store = nixcord_store::Store::new();
        store.apply(&GatewayEvent::Ready(Box::new(ready())));
        store.insert_history(ch, h.clone());
        let g = Snowflake(10);
        assert_eq!(store.member_color(g, Snowflake(1002)), Some(0xE74C3C));
        assert_eq!(
            store.member_nick(g, Snowflake(1005)),
            Some("Ferris the Crab")
        );
        assert_eq!(store.member_color(g, Snowflake(1004)), None);
        assert!(h.iter().filter(|m| store.is_mentioned(m)).count() >= 2);
    }

    #[test]
    fn dm_history_has_no_members() {
        let h = history_for(Snowflake(1), &people());
        assert!(h.iter().all(|m| m.member.is_none() && m.guild_id.is_none()));
    }
}

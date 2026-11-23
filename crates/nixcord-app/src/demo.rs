//! `--demo` backend: fake data fed through the exact same `AppEvent` path as the real
//! backend, with no network access. Used for screenshots and RAM measurements.
//!
//! The data deliberately exercises everything the model can carry: member roles and role
//! colors, nicknames, reactions (some by me), rich and link embeds, replies, mentions of
//! me (direct and via a role), unicode and custom emoji, and markdown-heavy content.
//! It also carries presences (every status, custom statuses, activities), friends, a
//! member list answered per subscription like Discord's GUILD_MEMBER_LIST_UPDATE, and
//! channel permissions (a read-only channel and hidden channels).

use crate::backend::{AppEvent, Command, EventTx, HISTORY_PAGE};
use crate::demo_assets::{avatar_hash, icon_hash, pool_avatar};
use crate::ui::search::SearchScope;
use nixcord_gateway::GatewayCommand;
use nixcord_model::permissions::{SEND_MESSAGES, VIEW_CHANNEL};
use nixcord_model::CustomEmoji;
use nixcord_model::{
    Activity, ActivityKind, Attachment, Channel, ChannelKind, Embed, EmbedAuthor, EmbedField,
    EmbedFooter, EmbedMedia, Emoji, ForumTag, GatewayEvent, Guild, GuildMember, ListMember,
    MemberListGroup, MemberListItem, MemberListOp, MemberListUpdate, Message, MessageReference,
    MessageUpdate, Overwrite, OverwriteKind, PartialMember, Presence, Reaction, ReadState, Ready,
    Relationship, RelationshipKind, Role, Snowflake, Status, ThreadMetadata, User,
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

/// Fake custom emoji ids (images come from `assets/demo/emoji`, see `crate::demo_assets`).
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
        avatar: cast_avatar(id),
        bot: id == BOT,
    }
}

/// Fake avatar hash of a cast member (mapped to `assets/demo/avatars`). Pip has none, so
/// the default avatar shows, as for many real users.
fn cast_avatar(id: u64) -> Option<String> {
    let file = match id {
        ME => "you.webp",
        BOT => "hearthbot.png",
        1002 => "sol.png",
        1003 => "quill.png",
        1005 => "ferris.png",
        1006 => "mira.png",
        1007 => "tamsin.webp",
        1008 => "otto.png",
        _ => return None,
    };
    avatar_hash(file)
}

fn people() -> Vec<User> {
    vec![
        user(ME, "you", Some("You")),
        user(BOT, "hearthbot", Some("hearthbot")),
        user(1002, "sol", Some("Sol")),
        user(1003, "quill", Some("Quill")),
        user(1004, "pip", None),
        user(1005, "ferris", Some("Ferris \u{1F980}")),
        user(1006, "mira_dev", Some("Mira \u{2728}")),
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
        ..Default::default()
    }
}

fn guild(id: u64, name: &str, channels: Vec<Channel>) -> Guild {
    let icon = match id {
        10 => "hearth.png",
        20 => "rust-lounge.png",
        30 => "pixel-art-club.png",
        40 => "synthwave.png",
        50 => "coffee-break.webp",
        _ => "",
    };
    let g = Snowflake(id);
    let role = |k: u64, name: &str, color: u32, position: i32| Role {
        id: role_id(g, k),
        name: name.into(),
        color,
        position,
        ..Default::default()
    };
    Guild {
        id: g,
        name: name.into(),
        icon: icon_hash(icon),
        channels,
        roles: vec![
            Role {
                id: g,
                name: "@everyone".into(),
                color: 0,
                position: 0,
                permissions: VIEW_CHANNEL | SEND_MESSAGES,
            },
            role(ROLE_HELPER, "helper", 0x2ECC71, 1),
            role(ROLE_DEV, "dev", 0xF1C40F, 2),
            role(ROLE_ARTIST, "artist", 0x9B59B6, 3),
            role(ROLE_MOD, "mod", 0xE74C3C, 4),
        ],
        unavailable: false,
        owner_id: (id == 10).then_some(Snowflake(1002)),
        // My own member: its roles drive the permission checks.
        members: vec![GuildMember {
            user: Some(user(ME, "you", Some("You"))),
            roles: member_for(Snowflake(ME), g).roles,
            ..Default::default()
        }],
        emojis: demo_guild_emojis(id),
        threads: Vec::new(),
    }
}

/// Custom emoji of the demo servers (images from `assets/demo/emoji`, see `Media`).
fn demo_guild_emojis(guild: u64) -> Vec<CustomEmoji> {
    let e = |id: u64, name: &str, animated: bool| CustomEmoji {
        id: Snowflake(id),
        name: name.into(),
        animated,
        available: true,
    };
    match guild {
        10 => vec![
            e(EMOJI_FERRIS, "ferris", false),
            e(EMOJI_PARTY, "party", true),
            e(EMOJI_PARTY + 1, "nixcord", false),
            e(EMOJI_PARTY + 2, "shipit", false),
            e(EMOJI_PARTY + 3, "heart_nix", false),
            e(EMOJI_PARTY + 4, "pepe_think", false),
            e(EMOJI_PARTY + 5, "catjam", true),
            e(EMOJI_PARTY + 6, "kekw", false),
            e(EMOJI_PARTY + 7, "pog", false),
            e(EMOJI_PARTY + 8, "sadge", false),
            // Locked (e.g. lost boost level): hidden from the picker.
            CustomEmoji {
                available: false,
                ..e(EMOJI_PARTY + 9, "locked", false)
            },
        ],
        20 => vec![
            e(EMOJI_PARTY + 20, "rustacean", false),
            e(EMOJI_PARTY + 21, "unsafe", false),
            e(EMOJI_PARTY + 22, "borrowck", false),
        ],
        _ => Vec::new(),
    }
}

fn ready() -> Ready {
    use ChannelKind::*;
    let t = Some;
    let mut nix = guild(
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
                "\u{1F4A1}-suggestions",
                t(101),
                1,
                Some("Ideas welcome"),
            ),
            chan(
                104,
                t(10),
                Text,
                "\u{1F41B}-bugs",
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
    // Permissions: #changelog is read-only for non-mods, #mod-only is hidden from me (I am
    // "dev"), and a literal "__hidden__" placeholder is always filtered out.
    let everyone = Snowflake(10);
    let mods = role_id(everyone, ROLE_MOD);
    let ow = |id: Snowflake, allow: u64, deny: u64| Overwrite {
        id,
        kind: OverwriteKind::Role,
        allow,
        deny,
    };
    nix.channels[0].permission_overwrites =
        vec![ow(everyone, 0, SEND_MESSAGES), ow(mods, SEND_MESSAGES, 0)];
    nix.channels.push(Channel {
        permission_overwrites: vec![ow(everyone, 0, VIEW_CHANNEL), ow(mods, VIEW_CHANNEL, 0)],
        ..chan(109, t(10), Text, "mod-only", t(105), 3, None)
    });
    nix.channels
        .push(chan(110, t(10), Text, "__hidden__", t(101), 3, None));
    // Active threads under #general (102) and #💡-suggestions (103).
    nix.threads = vec![
        Channel {
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: false,
            }),
            last_message_id: Some(snowflake_at(now_ms() - 4 * 60_000, 1)),
            ..chan(111, t(10), PublicThread, "v0.3-roadmap", t(102), 0, None)
        },
        Channel {
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: false,
            }),
            last_message_id: Some(snowflake_at(now_ms() - 12 * 60_000, 2)),
            ..chan(
                112,
                t(10),
                PublicThread,
                "compact-mode-ideas",
                t(103),
                0,
                None,
            )
        },
    ];
    let mut rust = guild(
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
            Channel {
                available_tags: vec![
                    ForumTag {
                        id: Snowflake(20401),
                        name: "Question".into(),
                        emoji_name: Some("\u{2753}".into()),
                        ..Default::default()
                    },
                    ForumTag {
                        id: Snowflake(20402),
                        name: "egui".into(),
                        emoji_name: Some("\u{1F3A8}".into()),
                        ..Default::default()
                    },
                    ForumTag {
                        id: Snowflake(20403),
                        name: "Async".into(),
                        emoji_name: Some("\u{26A1}".into()),
                        ..Default::default()
                    },
                    ForumTag {
                        id: Snowflake(20404),
                        name: "Solved".into(),
                        emoji_name: Some("\u{2705}".into()),
                        ..Default::default()
                    },
                ],
                ..chan(
                    204,
                    t(20),
                    Forum,
                    "help-forum",
                    t(200),
                    3,
                    Some("Ask questions, share patterns, and mark solved posts"),
                )
            },
            chan(205, t(20), Category, "Voice", None, 1, None),
            chan(206, t(20), Voice, "Pairing", t(205), 0, None),
        ],
    );
    rust.threads = vec![
        Channel {
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: false,
            }),
            applied_tags: vec![Snowflake(20401), Snowflake(20402)],
            last_message_id: Some(snowflake_at(now_ms() - 3 * 60_000, 11)),
            ..chan(
                207,
                t(20),
                PublicThread,
                "Keeping egui idle memory under 150 MB on Windows",
                t(204),
                0,
                None,
            )
        },
        Channel {
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: false,
            }),
            applied_tags: vec![Snowflake(20403), Snowflake(20404)],
            last_message_id: Some(snowflake_at(now_ms() - 25 * 60_000, 12)),
            ..chan(
                208,
                t(20),
                PublicThread,
                "Clean shutdown for tokio websocket tasks",
                t(204),
                0,
                None,
            )
        },
        Channel {
            thread_metadata: Some(ThreadMetadata {
                archived: false,
                locked: false,
            }),
            applied_tags: vec![Snowflake(20402)],
            last_message_id: Some(snowflake_at(now_ms() - 95 * 60_000, 13)),
            ..chan(
                209,
                t(20),
                PublicThread,
                "Custom font fallback order for crisp emoji rendering",
                t(204),
                0,
                None,
            )
        },
    ];
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
        ..Default::default()
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
        presences: people_presences(),
        relationships: {
            let mut rels: Vec<Relationship> = [1002u64, 1003, 1005, 1008]
                .iter()
                .map(|&id| Relationship {
                    id: Snowflake(id),
                    kind: RelationshipKind::Friend,
                    user: p.iter().find(|u| u.id.0 == id).cloned(),
                    nickname: None,
                })
                .collect();
            rels.push(Relationship {
                id: Snowflake(1006),
                kind: RelationshipKind::IncomingRequest,
                user: p.iter().find(|u| u.id.0 == 1006).cloned(),
                nickname: None,
            });
            rels.push(Relationship {
                id: Snowflake(1007),
                kind: RelationshipKind::OutgoingRequest,
                user: p.iter().find(|u| u.id.0 == 1007).cloned(),
                nickname: None,
            });
            rels.push(Relationship {
                id: Snowflake(1009),
                kind: RelationshipKind::Blocked,
                user: Some(user(1009, "bramble", Some("Bramble"))),
                nickname: None,
            });
            rels
        },
        status: Some(Status::Online),
        read_state: vec![
            ReadState {
                id: Snowflake(103),
                last_message_id: Some(snowflake_at(now_ms() - 30 * 60_000, 0)),
                mention_count: 1,
            },
            ReadState {
                id: Snowflake(207),
                last_message_id: Some(snowflake_at(now_ms() - 40 * 60_000, 0)),
                mention_count: 0,
            },
        ],
    }
}

const LINES: &[&str] = &[
    "Does the client remember which channel I had open after a restart?",
    "Pretty sure it does, the last channel is saved with the theme.",
    "not yet. right now it opens the DM list first, but restoring the last channel is on the list.",
    "would be a one-line change in the prefs struct honestly",
    "For some reason my scroll position jumps when an image loads. Am I doing something wrong?",
    "check settings -> chat. there is a toggle for reserving image space.",
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

/// Natural sizes of the demo images (must match the files in `assets/demo`).
const SHOT_W: u32 = 1200;
const SHOT_H: u32 = 675;
const GIF_CAT: (u32, u32) = (400, 180);
const GIF_LOADING: (u32, u32) = (200, 200);
const GIF_HORSE: (u32, u32) = (240, 160);

/// A photo attachment (the file is `assets/demo/photos/<name>`).
fn photo(t: u64, k: u64, name: &str, size: u64, (w, h): (u32, u32)) -> Attachment {
    Attachment {
        id: snowflake_at(t, 0x50_0000 + k),
        filename: name.into(),
        url: format!("https://cdn.discordapp.com/attachments/102/{k}/{name}"),
        size,
        content_type: Some("image/jpeg".into()),
        width: Some(w),
        height: Some(h),
    }
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
        image: Some(EmbedMedia {
            url: "https://cdn.discordapp.com/attachments/102/482/nixcord-chat.jpg".into(),
            proxy_url: None,
            width: Some(SHOT_W),
            height: Some(SHOT_H),
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
            url: "https://opengraph.githubassets.com/demo/emilk/egui/nixcord-dms.jpg".into(),
            proxy_url: None,
            width: Some(SHOT_W),
            height: Some(SHOT_H),
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
            "unicode \u{1F980}\u{1F680}\u{2728} flags \u{1F1EF}\u{1F1F5}\u{1F3F3}\u{FE0F}\u{200D}\u{1F308} \
             family \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} tones \u{1F44D}\u{1F3FD}\u{1F44B}\u{1F3FF} \
             keycap 1\u{FE0F}\u{20E3} \u{2764}\u{FE0F} \u{00A9}\u{FE0F} (plain #1 and \u{a9} stay text) \
             and custom <:ferris:{EMOJI_FERRIS}> <a:party:{EMOJI_PARTY}> in one line"
        ),
        t,
    );
    emoji.reactions = vec![
        reaction(custom_emoji(EMOJI_FERRIS, "ferris", false), 5, true),
        reaction(unicode("\u{1F980}"), 3, false),
        reaction(unicode("\u{1F1EF}\u{1F1F5}"), 2, false),
        reaction(unicode("\u{1F44D}\u{1F3FD}"), 1, true),
    ];
    out.push(emoji);

    // Emoji-only message: shown jumbo.
    let (id, t) = at(7);
    out.push(base_msg(
        id,
        channel,
        juan,
        "\u{1F980}\u{1F389}\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}".to_string(),
        t,
    ));

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

    // Photo attachments.
    let (id, t) = at(5);
    let mut desk = base_msg(
        id,
        channel,
        &people[7],
        "finally finished the desk setup. the cat approves (mostly)".into(),
        t,
    );
    desk.attachments = vec![
        photo(t, 0, "desk-setup.jpg", 88_580, (1200, 800)),
        photo(t, 1, "cat-on-keyboard.jpg", 121_036, (1200, 675)),
    ];
    desk.reactions = vec![
        reaction(unicode("\u{1F63B}"), 7, true),
        reaction(custom_emoji(EMOJI_PARTY + 3, "heart_nix", false), 2, false),
    ];
    out.push(desk);
    let (id, t) = at(4);
    let mut lake = base_msg(
        id,
        channel,
        &people[8],
        "weekend hike, no signal and no regrets".into(),
        t,
    );
    lake.attachments = vec![photo(t, 2, "lake-bohinj.jpg", 153_856, (1200, 801))];
    lake.reactions = vec![reaction(unicode("\u{1F3D4}\u{FE0F}"), 4, false)];
    out.push(lake);

    // Message components: a Components V2 rules post and a legacy action row.
    let (id, t) = at(3);
    out.push(rules_post(id, channel, bot, t, dev));
    let (id, t) = at(2);
    out.push(vote_buttons(id, channel, bot, t));

    // GIFs: two attachments and a Tenor gifv embed (files from `assets/demo/gifs`).
    let (id, t) = at(1);
    let mut gifs = base_msg(
        id,
        channel,
        rebrax,
        format!("gif test <a:party:{EMOJI_PARTY}>"),
        t,
    );
    for (k, (name, (w, h), size)) in [
        ("cat-gallop.gif", GIF_CAT, 381_419u64),
        ("loading.gif", GIF_LOADING, 21_258),
    ]
    .into_iter()
    .enumerate()
    {
        gifs.attachments.push(Attachment {
            id: snowflake_at(t, 0x47_0000 + k as u64),
            filename: name.into(),
            url: format!("https://cdn.discordapp.com/attachments/104/{k}/{name}"),
            size,
            content_type: Some("image/gif".into()),
            width: Some(w),
            height: Some(h),
        });
    }
    let tenor = |code: &str, ext: &str| {
        format!("https://media.tenor.com/nixDemoHorseGallop{code}/horse-gallop.{ext}")
    };
    gifs.embeds.push(Embed {
        kind: Some("gifv".into()),
        url: Some("https://tenor.com/view/horse-gallop-gif-1234567".into()),
        provider: Some(EmbedAuthor {
            name: "Tenor".into(),
            url: Some("https://tenor.co".into()),
            icon_url: None,
        }),
        thumbnail: Some(EmbedMedia {
            url: tenor("Ae", "png"),
            proxy_url: None,
            width: Some(GIF_HORSE.0),
            height: Some(GIF_HORSE.1),
        }),
        video: Some(EmbedMedia {
            url: tenor("Po", "mp4"),
            proxy_url: None,
            width: Some(GIF_HORSE.0),
            height: Some(GIF_HORSE.1),
        }),
        ..Default::default()
    });
    gifs.reactions = vec![reaction(custom_emoji(EMOJI_PARTY, "party", true), 3, true)];
    out.push(gifs);

    // Videos (see `crate::video`): opt-in, since demo mode has no network.
    let (id, t) = at(0);
    out.extend(demo_videos(channel, mira, id, t));

    out
}

/// A local video when `NIXCORD_DEMO_VIDEO=<path>` is set, and a small public test clip when
/// `NIXCORD_DEMO_NET=1` allows the network.
fn demo_videos(channel: Snowflake, author: &User, id: Snowflake, t: u64) -> Vec<Message> {
    let mut out = Vec::new();
    let video = |k: u64, filename: String, url: String, size: u64, wh: Option<(u32, u32)>| {
        let mut m = base_msg(
            Snowflake(id.0 + k),
            channel,
            author,
            format!("video test: {filename}"),
            t,
        );
        m.attachments.push(Attachment {
            id: Snowflake(id.0 + 0x100 + k),
            filename,
            url,
            size,
            content_type: Some("video/mp4".into()),
            width: wh.map(|d| d.0),
            height: wh.map(|d| d.1),
        });
        m
    };
    if let Some(path) = std::env::var_os("NIXCORD_DEMO_VIDEO").map(std::path::PathBuf::from) {
        let name = path
            .file_name()
            .map_or_else(|| "video.mp4".into(), |n| n.to_string_lossy().into_owned());
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let url = format!("{}{name}", crate::video::DEMO_VIDEO_URL);
        out.push(video(0, name, url.clone(), size, None));
        // The same file as a `gifv` embed: muted, looping, no controls.
        let mut gifv = base_msg(Snowflake(id.0 + 2), channel, author, "gifv test".into(), t);
        gifv.embeds.push(Embed {
            kind: Some("gifv".into()),
            url: Some(url.clone()),
            video: Some(EmbedMedia {
                url,
                proxy_url: None,
                width: Some(320),
                height: Some(180),
            }),
            ..Default::default()
        });
        out.push(gifv);
    }
    if std::env::var_os("NIXCORD_DEMO_NET").is_some_and(|v| v == "1") {
        let url = crate::video::DEMO_NET_VIDEO.to_owned();
        out.push(video(
            1,
            "flower.mp4".into(),
            url,
            1_128_375,
            Some((960, 540)),
        ));
    }
    out
}

/// Components V2 rules post: container with accent bar, banner, text, separators, buttons.
fn rules_post(id: Snowflake, channel: Snowflake, bot: &User, t: u64, dev: Snowflake) -> Message {
    use nixcord_model::components::*;
    let media = |url: &str, width: u32, height: u32| UnfurledMedia {
        url: url.into(),
        width: Some(width),
        height: Some(height),
        content_type: Some("image/png".into()),
        ..Default::default()
    };
    let text = |s: String| Component::TextDisplay(TextDisplay { content: s });
    let sep = |divider: bool, spacing: u8| Component::Separator(Separator { divider, spacing });
    let link = |label: &str, emoji: Emoji, url: &str| {
        Component::Button(Button {
            style: ButtonStyle::Link,
            label: Some(label.into()),
            emoji: Some(emoji),
            url: Some(url.into()),
            ..Default::default()
        })
    };
    let mut m = base_msg(id, channel, bot, String::new(), t);
    m.flags = IS_COMPONENTS_V2;
    m.components = vec![Component::Container(Container {
        accent_color: Some(0x9B59B6),
        spoiler: false,
        components: vec![
            Component::MediaGallery(MediaGallery {
                items: vec![MediaGalleryItem {
                    media: media("https://cdn.example.com/nix/hearth-banner.jpg", 1200, 400),
                    description: Some("Hearth banner".into()),
                    spoiler: false,
                }],
            }),
            text("# \u{1F4DC} Server Rules\nWelcome to **Hearth**! Read these before chatting in <#102>.".into()),
            sep(true, 1),
            text(format!(
                "### 1. Be respectful\nNo harassment, hate speech or personal attacks. \
                 <:ferris:{EMOJI_FERRIS}>\n### 2. Stay on topic\nBot help goes in <#104>, \
                 ideas in <#103>.\n### 3. No spam\n- no unsolicited DMs or ads\n- no mass mentions"
            )),
            sep(true, 2),
            Component::Section(Section {
                components: vec![text(format!(
                    "**Need help?**\nAsk in <#104> or ping <@&{dev}>. Breaking the rules gets \
                     you a warning, then a timeout."
                ))],
                accessory: Some(Box::new(Component::Thumbnail(Thumbnail {
                    media: media("https://cdn.example.com/nix/mod-badge.png", 128, 128),
                    description: Some("Moderators".into()),
                    spoiler: false,
                }))),
            }),
            sep(false, 1),
            Component::ActionRow(ActionRow {
                components: vec![
                    link("Terms of Service", unicode("\u{1F4DC}"), "https://discord.com/terms"),
                    link("Guidelines", unicode("\u{1F6E1}"), "https://discord.com/guidelines"),
                    link(
                        "Source",
                        custom_emoji(EMOJI_FERRIS, "ferris", false),
                        "https://github.com/emilk/egui",
                    ),
                ],
            }),
        ],
    })];
    m
}

/// Legacy (non-V2) message: content plus action rows of buttons and a select.
fn vote_buttons(id: Snowflake, channel: Snowflake, bot: &User, t: u64) -> Message {
    use nixcord_model::components::*;
    let button = |style: ButtonStyle, label: &str, emoji: Option<Emoji>, disabled: bool| {
        Component::Button(Button {
            style,
            label: Some(label.into()),
            emoji,
            custom_id: Some(label.to_lowercase()),
            disabled,
            ..Default::default()
        })
    };
    let mut m = base_msg(
        id,
        channel,
        bot,
        "**Poll:** what should nixcord build next?".into(),
        t,
    );
    m.components = vec![
        Component::ActionRow(ActionRow {
            components: vec![
                button(
                    ButtonStyle::Primary,
                    "Voice",
                    Some(unicode("\u{1F3A4}")),
                    false,
                ),
                button(ButtonStyle::Secondary, "Threads", None, false),
                button(ButtonStyle::Success, "Both", None, false),
                button(ButtonStyle::Danger, "Neither", None, true),
            ],
        }),
        Component::ActionRow(ActionRow {
            components: vec![Component::Select(SelectMenu {
                kind: SelectKind::String,
                custom_id: Some("theme".into()),
                placeholder: Some("Pick your favorite theme".into()),
                options: vec![
                    SelectOption {
                        label: "Graphite".into(),
                        value: "graphite".into(),
                        description: Some("Flat dark surfaces (default)".into()),
                        default: true,
                        ..Default::default()
                    },
                    SelectOption {
                        label: "Onyx".into(),
                        value: "onyx".into(),
                        description: Some("Pure black with hairline borders".into()),
                        ..Default::default()
                    },
                    SelectOption {
                        label: "Aurora".into(),
                        value: "aurora".into(),
                        description: Some("Deep teal and mint gradient".into()),
                        ..Default::default()
                    },
                    SelectOption {
                        label: "Sunset".into(),
                        value: "sunset".into(),
                        description: Some("Warm plum and coral gradient".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })],
        }),
        Component::ActionRow(ActionRow {
            components: vec![Component::Select(SelectMenu {
                kind: SelectKind::User,
                custom_id: Some("assign_reviewer".into()),
                placeholder: Some("Assign a reviewer".into()),
                ..Default::default()
            })],
        }),
        Component::ActionRow(ActionRow {
            components: vec![Component::Select(SelectMenu {
                kind: SelectKind::Channel,
                custom_id: Some("announce_channel".into()),
                placeholder: Some("Pick an announcement channel".into()),
                ..Default::default()
            })],
        }),
    ];
    m
}

// ---- presences and member lists ---------------------------------------------------------

fn activity(kind: ActivityKind, name: &str, state: Option<&str>, emoji: Option<&str>) -> Activity {
    Activity {
        kind,
        name: name.into(),
        state: state.map(Into::into),
        details: (kind == ActivityKind::Streaming).then(|| "Speedrunning Celeste".into()),
        emoji: emoji.map(unicode),
        url: None,
    }
}

/// Fake presence of a demo user (`None` = offline).
fn presence_of(id: u64) -> Option<Presence> {
    use ActivityKind::*;
    let (status, activities) = match id {
        ME => (Status::Online, vec![]),
        BOT => (Status::Online, vec![activity(Playing, "/help", None, None)]),
        1002 => (
            Status::Online,
            vec![activity(
                Custom,
                "Custom Status",
                Some("rewriting it in Rust"),
                Some("\u{2615}"),
            )],
        ),
        1003 => (
            Status::Online,
            vec![activity(Playing, "Factorio", None, None)],
        ),
        1004 => (Status::Idle, vec![]),
        1005 => (
            Status::Online,
            vec![activity(Listening, "Spotify", None, None)],
        ),
        1006 => (
            Status::Dnd,
            vec![activity(
                Custom,
                "Custom Status",
                Some("deep focus \u{1F3A7}\u{2615}"),
                Some("\u{1F9E0}"),
            )],
        ),
        1007 => return None,
        1008 => (
            Status::Online,
            vec![activity(Streaming, "Twitch", None, None)],
        ),
        n => match n % 6 {
            0 => (Status::Online, vec![]),
            1 => (Status::Idle, vec![]),
            2 => (Status::Dnd, vec![]),
            3 => (
                Status::Online,
                vec![activity(Watching, "YouTube Together", None, None)],
            ),
            _ => return None,
        },
    };
    Some(Presence {
        user_id: Snowflake(id),
        status,
        activities,
        ..Default::default()
    })
}

fn people_presences() -> Vec<Presence> {
    people()
        .iter()
        .filter(|u| u.id.0 != ME)
        .filter_map(|u| presence_of(u.id.0))
        .collect()
}

const EXTRA_NAMES: &[&str] = &[
    "aurora", "basil", "cypress", "delta", "ember", "fjord", "garnet", "harbor", "indigo",
    "juniper", "kestrel", "lumen", "marlow", "nimbus", "onyx", "pixel", "quill", "raven", "sable",
    "tundra", "umber", "vesper", "willow", "xenon", "yarrow", "zephyr", "acorn", "bramble",
    "cinder", "dune", "echo", "flint", "glade", "hollow", "iris", "jasper",
];

/// Everyone in a demo guild: the cast plus generated members (more in bigger guilds).
fn roster(guild: Snowflake) -> Vec<(User, PartialMember)> {
    let extra = match guild.0 {
        10 => EXTRA_NAMES.len(),
        20 => 20,
        _ => 8,
    };
    let mut out: Vec<(User, PartialMember)> = people()
        .into_iter()
        .map(|u| {
            let m = member_for(u.id, guild);
            (u, m)
        })
        .collect();
    for (i, name) in EXTRA_NAMES.iter().take(extra).enumerate() {
        let id = 5000 + i as u64;
        let roles = if i % 4 == 0 {
            vec![role_id(guild, ROLE_HELPER)]
        } else {
            Vec::new()
        };
        let u = User {
            avatar: pool_avatar(i),
            ..user(id, name, None)
        };
        out.push((
            u,
            PartialMember {
                roles,
                ..Default::default()
            },
        ));
    }
    out
}

/// The full member list of a demo guild, as Discord lays it out: hoisted roles (mod,
/// dev) for online members, then "Online", then "Offline"; names sorted in each group.
fn member_list_rows(guild: Snowflake) -> (Vec<MemberListItem>, Vec<MemberListGroup>, u32) {
    let hoisted = [ROLE_MOD, ROLE_DEV];
    let mut groups: Vec<(String, Vec<MemberListItem>)> = hoisted
        .iter()
        .map(|&k| (role_id(guild, k).to_string(), Vec::new()))
        .chain([
            ("online".to_string(), Vec::new()),
            ("offline".to_string(), Vec::new()),
        ])
        .collect();
    let mut people = roster(guild);
    let name = |u: &User, m: &PartialMember| {
        m.nick
            .clone()
            .unwrap_or_else(|| u.display_name().to_string())
            .to_lowercase()
    };
    people.sort_by_key(|(u, m)| name(u, m));
    let mut online = 0;
    for (u, m) in people {
        let presence = presence_of(u.id.0);
        let slot = match &presence {
            None => groups.len() - 1,
            Some(_) => {
                online += 1;
                hoisted
                    .iter()
                    .position(|&k| m.roles.contains(&role_id(guild, k)))
                    .unwrap_or(groups.len() - 2)
            }
        };
        let item = MemberListItem::Member(Box::new(ListMember {
            member: GuildMember {
                user: Some(u),
                roles: m.roles,
                nick: m.nick,
                avatar: None,
                ..Default::default()
            },
            presence,
        }));
        groups[slot].1.push(item);
    }
    let mut rows = Vec::new();
    let mut summary = Vec::new();
    for (id, members) in groups {
        if members.is_empty() {
            continue;
        }
        let g = MemberListGroup {
            id,
            count: members.len() as u32,
        };
        summary.push(g.clone());
        rows.push(MemberListItem::Group(g));
        rows.extend(members);
    }
    (rows, summary, online)
}

/// Answer a subscription like Discord: one SYNC per requested range.
fn member_list_sync(guild: Snowflake, ranges: &[(u32, u32)]) -> GatewayEvent {
    let (rows, groups, online) = member_list_rows(guild);
    let total = groups.iter().map(|g| g.count).sum();
    let ops = ranges
        .iter()
        .map(|&(a, b)| MemberListOp::Sync {
            range: (a, b),
            items: rows
                .iter()
                .skip(a as usize)
                .take((b - a + 1) as usize)
                .cloned()
                .collect(),
        })
        .collect();
    GatewayEvent::MemberListUpdate(Box::new(MemberListUpdate {
        id: "everyone".into(),
        guild_id: guild,
        member_count: total,
        online_count: online,
        groups,
        ops,
    }))
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
            let posts: [(u64, usize, &str, bool); 3] = [
                (
                    104,
                    3,
                    "found a crash when the window is minimized, logs incoming — <@1000> can you check?",
                    true,
                ),
                (201, 5, "anyone tried the new borrow checker diagnostics?", false),
                (2, 3, "you around? got a minute", false),
            ];
            for (i, (channel, who, text, ping_me)) in posts.into_iter().enumerate() {
                let t = now_ms();
                let mut m = base_msg(
                    snowflake_at(t, 0x3f_0000 + i as u64),
                    Snowflake(channel),
                    &people[who.min(people.len() - 1)],
                    text.into(),
                    t,
                );
                if ping_me {
                    m.mentions.push(people[0].clone());
                }
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
                attachments,
            } => {
                seq += 1;
                let t = now_ms();
                let mut mine = base_msg(snowflake_at(t, seq), channel, &people[0], content, t);
                mine.attachments = attachments
                    .into_iter()
                    .enumerate()
                    .map(|(idx, f)| Attachment {
                        id: snowflake_at(t, seq + idx as u64 + 100),
                        url: format!("https://demo.invalid/attachments/{}", f.filename),
                        size: f.bytes.len() as u64,
                        content_type: Some(f.content_type),
                        filename: f.filename,
                        width: None,
                        height: None,
                    })
                    .collect();
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
                let echo_text = if mine.content.is_empty() && !mine.attachments.is_empty() {
                    format!("received {} attachment(s)", mine.attachments.len())
                } else {
                    format!("echo: {}", mine.content)
                };
                let mut echo = base_msg(snowflake_at(t, seq), channel, &bot, echo_text, t);
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
            Command::Typing { .. } | Command::Ack { .. } => {}
            Command::CreateDm { recipient_id } => {
                let target = people
                    .iter()
                    .find(|u| u.id == recipient_id)
                    .cloned()
                    .unwrap_or_else(|| user(recipient_id.0, "friend", Some("Friend")));
                seq += 1;
                let ch = Channel {
                    id: Snowflake(9000 + recipient_id.0),
                    kind: ChannelKind::Dm,
                    recipients: vec![target],
                    last_message_id: Some(snowflake_at(now_ms(), seq)),
                    ..Default::default()
                };
                events.send(AppEvent::DmCreated(Box::new(ch)));
            }
            Command::AcceptFriendRequest { user_id } => {
                let u = people.iter().find(|u| u.id == user_id).cloned();
                events.send(AppEvent::Gateway(GatewayEvent::RelationshipAdd(Box::new(
                    Relationship {
                        id: user_id,
                        kind: RelationshipKind::Friend,
                        user: u,
                        nickname: None,
                    },
                ))));
            }
            Command::RemoveRelationship { user_id } => {
                events.send(AppEvent::Gateway(GatewayEvent::RelationshipRemove {
                    user_id,
                }));
            }
            Command::BlockUser { user_id } => {
                let u = people.iter().find(|u| u.id == user_id).cloned();
                events.send(AppEvent::Gateway(GatewayEvent::RelationshipAdd(Box::new(
                    Relationship {
                        id: user_id,
                        kind: RelationshipKind::Blocked,
                        user: u,
                        nickname: None,
                    },
                ))));
            }
            Command::SendFriendRequest { username } => {
                let clean = username.trim().to_string();
                if clean.eq_ignore_ascii_case("you") {
                    events.send(AppEvent::FriendRequestResult {
                        username: clean,
                        error: Some("You can't send a friend request to yourself.".into()),
                    });
                } else {
                    seq += 1;
                    let target = people
                        .iter()
                        .find(|u| u.username.eq_ignore_ascii_case(&clean))
                        .cloned()
                        .unwrap_or_else(|| user(1200 + seq, &clean, Some(&clean)));
                    events.send(AppEvent::Gateway(GatewayEvent::RelationshipAdd(Box::new(
                        Relationship {
                            id: target.id,
                            kind: RelationshipKind::OutgoingRequest,
                            user: Some(target),
                            nickname: None,
                        },
                    ))));
                    events.send(AppEvent::FriendRequestResult {
                        username: clean,
                        error: None,
                    });
                }
            }
            Command::SearchMessages {
                request_id,
                scope,
                query,
            } => {
                let demo_ready = ready();
                let chan_ids: Vec<Snowflake> = match scope {
                    SearchScope::Guild(gid) => demo_ready
                        .guilds
                        .iter()
                        .find(|g| g.id == gid)
                        .map(|g| {
                            g.channels
                                .iter()
                                .filter(|c| c.kind.is_textual() && c.id.0 != 109 && c.id.0 != 110)
                                .chain(g.threads.iter())
                                .map(|c| c.id)
                                .collect()
                        })
                        .unwrap_or_default(),
                    SearchScope::Channel(cid) => vec![cid],
                };
                for cid in &chan_ids {
                    histories
                        .entry(*cid)
                        .or_insert_with(|| history_for(*cid, &people));
                }
                let terms: Vec<String> = query
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .split_whitespace()
                    .map(|s| s.to_lowercase())
                    .collect();
                let all_msgs: Vec<&Message> = chan_ids
                    .iter()
                    .filter_map(|cid| histories.get(cid))
                    .flat_map(|v| v.iter())
                    .collect();
                let result = crate::ui::search::local_search(
                    all_msgs,
                    &query,
                    &terms,
                    query.offset.unwrap_or(0),
                );
                let events = events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    events.send(AppEvent::SearchResults { request_id, result });
                });
            }
            Command::LoadHistoryAround { channel, around } => {
                let all = histories
                    .entry(channel)
                    .or_insert_with(|| history_for(channel, &people));
                let pos = all
                    .iter()
                    .position(|m| m.id == around)
                    .unwrap_or(all.len().saturating_sub(1));
                let half = HISTORY_PAGE as usize / 2;
                let start = pos.saturating_sub(half);
                let end = (start + HISTORY_PAGE as usize).min(all.len());
                let page = all[start..end].to_vec();
                let events = events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    events.send(AppEvent::HistoryAround {
                        channel,
                        around,
                        messages: page,
                    });
                });
            }
            Command::CreateThread {
                parent_id,
                is_forum: _,
                name,
                message,
                applied_tags,
            } => {
                seq += 1;
                let t = now_ms();
                let tid = Snowflake(8000 + seq);
                let gid = guild_of(parent_id);
                let first_msg = message
                    .filter(|s| !s.trim().is_empty())
                    .map(|body| base_msg(snowflake_at(t, seq + 1), tid, &people[0], body, t));
                let thread = Channel {
                    id: tid,
                    kind: ChannelKind::PublicThread,
                    guild_id: gid,
                    name: Some(name),
                    parent_id: Some(parent_id),
                    last_message_id: first_msg.as_ref().map(|m| m.id),
                    thread_metadata: Some(ThreadMetadata {
                        archived: false,
                        locked: false,
                    }),
                    applied_tags,
                    ..Default::default()
                };
                if let Some(m) = first_msg {
                    histories.insert(tid, vec![m]);
                } else {
                    histories.insert(tid, Vec::new());
                }
                events.send(AppEvent::ThreadCreated(Box::new(thread)));
            }
            Command::UpdateThread {
                thread_id,
                archived,
                locked,
            } => {
                let demo_ready = ready();
                let mut ch = demo_ready
                    .guilds
                    .into_iter()
                    .flat_map(|g| g.threads.into_iter())
                    .find(|t| t.id == thread_id)
                    .unwrap_or_else(|| Channel {
                        id: thread_id,
                        kind: ChannelKind::PublicThread,
                        guild_id: Some(Snowflake(10)),
                        name: Some(format!("thread-{thread_id}")),
                        parent_id: Some(Snowflake(SHOWCASE_CHANNEL)),
                        ..Default::default()
                    });
                let meta = ch.thread_metadata.get_or_insert(ThreadMetadata {
                    archived: false,
                    locked: false,
                });
                if let Some(a) = archived {
                    meta.archived = a;
                }
                if let Some(l) = locked {
                    meta.locked = l;
                }
                events.send(AppEvent::Gateway(GatewayEvent::ThreadUpdate(Box::new(ch))));
            }
            Command::Interact(ix) => {
                let events = events.clone();
                let summary = if ix.values.is_empty() {
                    format!(
                        "**Poll:** what should nixcord build next?\n-# Latest vote: **{}** (by You)",
                        ix.custom_id
                    )
                } else {
                    format!(
                        "**Poll:** what should nixcord build next?\n-# Selected theme: **{}** (by You)",
                        ix.values.join(", ")
                    )
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(220)).await;
                    events.send(AppEvent::InteractionDone {
                        message_id: ix.message_id,
                        custom_id: ix.custom_id,
                        error: None,
                    });
                    events.send(AppEvent::Gateway(GatewayEvent::MessageUpdate(Box::new(
                        MessageUpdate {
                            id: ix.message_id,
                            channel_id: ix.channel_id,
                            content: Some(summary),
                            ..Default::default()
                        },
                    ))));
                });
            }
            Command::Gateway(GatewayCommand::SubscribeGuild {
                guild_id, ranges, ..
            }) => {
                if ranges.is_empty() {
                    continue;
                }
                let events = events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    events.send(AppEvent::Gateway(member_list_sync(guild_id, &ranges)));
                });
            }
            // The store already shows the new status; Discord would echo it through
            // SESSIONS_REPLACE.
            Command::Gateway(GatewayCommand::SetPresence { status, .. }) => {
                events.send(AppEvent::Gateway(GatewayEvent::OwnStatus(status)));
            }
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
    fn demo_permissions_and_member_list() {
        let mut store = nixcord_store::Store::new();
        store.apply(&GatewayEvent::Ready(Box::new(ready())));
        let g = Snowflake(10);
        let visible: Vec<u64> = store
            .visible_guild_channels(g)
            .iter()
            .map(|c| c.id.0)
            .collect();
        assert!(!visible.contains(&109), "mod-only hidden");
        assert!(!visible.contains(&110), "__hidden__ hidden");
        assert!(visible.contains(&102));
        assert!(store.can_view(Snowflake(100)) && !store.can_send(Snowflake(100)));
        assert!(store.can_send(Snowflake(102)));
        assert_eq!(store.status(Snowflake(1006)), Status::Dnd);
        assert_eq!(store.friends().len(), 4);

        store.set_member_list_focus(Some(g));
        store.apply(&member_list_sync(g, &[(0, 99)]));
        let list = store.member_list(g).unwrap();
        let sections = list.sections();
        let ids: Vec<&str> = sections.iter().map(|s| s.group.id.as_str()).collect();
        let mods = role_id(g, ROLE_MOD).to_string();
        let devs = role_id(g, ROLE_DEV).to_string();
        assert_eq!(ids, vec![mods.as_str(), devs.as_str(), "online", "offline"]);
        assert!(sections
            .iter()
            .all(|s| s.group.count as usize >= s.members.len()));
        assert_eq!(
            store.status_text(Snowflake(1002)).as_deref(),
            Some("\u{2615} rewriting it in Rust")
        );
    }

    #[test]
    fn dm_history_has_no_members() {
        let h = history_for(Snowflake(1), &people());
        assert!(h.iter().all(|m| m.member.is_none() && m.guild_id.is_none()));
    }
}

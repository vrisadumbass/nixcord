//! `--demo` backend: fake data fed through the exact same `AppEvent` path as the real
//! backend, with no network access. Used for screenshots and RAM measurements.

use crate::backend::{AppEvent, Command, EventTx, HISTORY_PAGE};
use nixcord_model::{
    Attachment, Channel, ChannelKind, GatewayEvent, Guild, Message, MessageUpdate, Ready, Role,
    Snowflake, User,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
const ME: u64 = 1000;
const BOT: u64 = 1001;

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
    Guild {
        id: Snowflake(id),
        name: name.into(),
        icon: None,
        channels,
        roles: vec![
            Role {
                id: Snowflake(id),
                name: "@everyone".into(),
                color: 0,
                position: 0,
            },
            Role {
                id: Snowflake(id + 1),
                name: "dev".into(),
                color: 0xF1C40F,
                position: 2,
            },
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

fn history_for(channel: Snowflake, people: &[User], me_mentions: bool) -> Vec<Message> {
    // Deterministic per channel: 120 messages spread over the last ~2 days.
    let n = 120u64;
    let now = now_ms();
    let span = 40 * 3600 * 1000u64;
    let mut seed = channel.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut out: Vec<Message> = Vec::with_capacity(n as usize);
    let mut t = now - span;
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
        let t = t.min(now - (n - i) * 1000);
        if r % 3 == 0 {
            author = 2 + (next() as usize % (people.len() - 2));
        }
        let a = if r % 11 == 0 { 0 } else { author };
        let mut content = LINES[(next() as usize) % LINES.len()].to_string();
        let mut mentions = Vec::new();
        if me_mentions && i == n - 6 {
            content = format!("<@{ME}> could you take a look at this when you have a minute?");
            mentions.push(people[0].clone());
        }
        let id = snowflake_at(t, i);
        let mut m = Message {
            id,
            channel_id: channel,
            guild_id: None,
            author: people[a].clone(),
            content,
            timestamp: iso(t),
            edited_timestamp: (r % 13 == 0).then(|| iso(t + 30_000)),
            attachments: Vec::new(),
            mentions,
            referenced_message: None,
        };
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
        if r % 7 == 0 {
            if let Some(prev) = out.iter().rev().nth(3) {
                let mut p = prev.clone();
                p.referenced_message = None;
                m.referenced_message = Some(Box::new(p));
            }
        }
        out.push(m);
    }
    out
}

pub async fn run(mut commands: mpsc::UnboundedReceiver<Command>, events: EventTx) {
    let people = people();
    let mut histories: HashMap<Snowflake, Vec<Message>> = HashMap::new();
    let mut seq: u64 = 0;

    events.send(AppEvent::LoginOk(people[0].clone()));
    events.send(AppEvent::Gateway(GatewayEvent::Connecting));
    tokio::time::sleep(Duration::from_millis(150)).await;
    events.send(AppEvent::Gateway(GatewayEvent::Ready(Box::new(ready()))));

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Login(_) => {
                events.send(AppEvent::LoginOk(people[0].clone()));
                events.send(AppEvent::Gateway(GatewayEvent::Ready(Box::new(ready()))));
            }
            Command::Logout => {}
            Command::LoadHistory { channel, before } => {
                let all = histories
                    .entry(channel)
                    .or_insert_with(|| history_for(channel, &people, channel.0 == 102));
                let mut page: Vec<Message> = all
                    .iter()
                    .filter(|m| before.is_none_or(|b| m.id < b))
                    .rev()
                    .take(HISTORY_PAGE as usize)
                    .cloned()
                    .collect();
                for m in &mut page {
                    m.channel_id = channel;
                }
                let events = events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    events.send(AppEvent::History {
                        channel,
                        messages: page,
                    });
                });
            }
            Command::Send { channel, content } => {
                seq += 1;
                let t = now_ms();
                let mine = Message {
                    id: snowflake_at(t, seq),
                    channel_id: channel,
                    guild_id: None,
                    author: people[0].clone(),
                    content: content.clone(),
                    timestamp: iso(t),
                    edited_timestamp: None,
                    attachments: Vec::new(),
                    mentions: Vec::new(),
                    referenced_message: None,
                };
                events.send(AppEvent::Sent(Box::new(mine.clone())));
                if content.trim_start().starts_with("/forbidden") {
                    events.send(AppEvent::Forbidden { channel });
                    continue;
                }
                // The demo bot "types" and then echoes the message back as a reply.
                let events = events.clone();
                let bot = people[1].clone();
                seq += 1;
                let reply_seq = seq;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    events.send(AppEvent::Gateway(GatewayEvent::TypingStart {
                        channel_id: channel,
                        user_id: bot.id,
                    }));
                    tokio::time::sleep(Duration::from_millis(2200)).await;
                    let t = now_ms();
                    let reply = Message {
                        id: snowflake_at(t, reply_seq),
                        channel_id: channel,
                        guild_id: None,
                        author: bot,
                        content: format!("echo: {}", mine.content),
                        timestamp: iso(t),
                        edited_timestamp: None,
                        attachments: Vec::new(),
                        mentions: vec![mine.author.clone()],
                        referenced_message: Some(Box::new(mine)),
                    };
                    events.send(AppEvent::Gateway(GatewayEvent::MessageCreate(Box::new(
                        reply,
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
                        attachments: None,
                    },
                ))));
            }
            Command::Delete { channel, message } => {
                events.send(AppEvent::Gateway(GatewayEvent::MessageDelete {
                    id: message,
                    channel_id: channel,
                }));
            }
            Command::Typing { .. } => {}
        }
    }
}

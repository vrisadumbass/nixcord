//! Message context menu state: pin / manage permissions, "Mark Unread" (moving the read
//! marker backwards with a mention recount) and `pinned` from MESSAGE_UPDATE.
//! Invented data only.

use super::*;
use nixcord_model::permissions::{MANAGE_MESSAGES, PIN_MESSAGES, SEND_MESSAGES, VIEW_CHANNEL};
use nixcord_model::{GuildMember, Ready};

const ME: u64 = 1;
const OTHER: u64 = 2;
const G: u64 = 900;
const ROLE_MOD: u64 = 901;
const ROLE_PIN: u64 = 902;
const TEXT: u64 = 10;
const DM: u64 = 50;

fn sf(n: u64) -> Snowflake {
    Snowflake(n)
}

fn user(id: u64) -> User {
    User {
        id: sf(id),
        username: format!("user{id}"),
        ..Default::default()
    }
}

fn role(id: u64, permissions: u64) -> Role {
    Role {
        id: sf(id),
        name: format!("r{id}"),
        permissions,
        ..Default::default()
    }
}

/// Guild G with one text channel and a DM; @everyone may view and send. `my_roles: None`
/// leaves my member unknown (permissions fail open).
fn store(my_roles: Option<&[u64]>) -> Store {
    let guild = Guild {
        id: sf(G),
        name: "g".into(),
        channels: vec![Channel {
            id: sf(TEXT),
            kind: ChannelKind::Text,
            guild_id: Some(sf(G)),
            ..Default::default()
        }],
        roles: vec![
            role(G, VIEW_CHANNEL | SEND_MESSAGES),
            role(ROLE_MOD, MANAGE_MESSAGES),
            role(ROLE_PIN, PIN_MESSAGES),
        ],
        members: my_roles
            .map(|r| {
                vec![GuildMember {
                    user: Some(user(ME)),
                    roles: r.iter().copied().map(sf).collect(),
                    ..Default::default()
                }]
            })
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(ME),
        guilds: vec![guild],
        private_channels: vec![Channel {
            id: sf(DM),
            kind: ChannelKind::Dm,
            recipients: vec![user(OTHER)],
            ..Default::default()
        }],
        session_id: "s".into(),
        ..Default::default()
    })));
    s
}

fn msg(id: u64, channel: u64, author: u64, mentions_me: bool) -> Message {
    Message {
        id: sf(id),
        channel_id: sf(channel),
        guild_id: (channel == TEXT).then_some(sf(G)),
        author: user(author),
        content: format!("m{id}"),
        timestamp: "2026-01-01T00:00:00+00:00".into(),
        mentions: if mentions_me {
            vec![user(ME)]
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}

/// History 100..=105 in `channel`, read up to the newest. 102 and 104 mention me.
fn with_history(s: &mut Store, channel: u64) {
    let msgs = (100..=105)
        .map(|id| msg(id, channel, OTHER, id == 102 || id == 104))
        .collect();
    s.insert_history(sf(channel), msgs);
    s.mark_read(sf(channel));
    assert!(!s.channel_unread(sf(channel)));
}

#[test]
fn pin_and_manage_permissions() {
    // Plain member: neither.
    let s = store(Some(&[]));
    assert!(!s.can_pin_messages(sf(TEXT)));
    assert!(!s.can_manage_messages(sf(TEXT)));
    // MANAGE_MESSAGES: both.
    let s = store(Some(&[ROLE_MOD]));
    assert!(s.can_pin_messages(sf(TEXT)));
    assert!(s.can_manage_messages(sf(TEXT)));
    // PIN_MESSAGES alone: pin, but not delete others' messages.
    let s = store(Some(&[ROLE_PIN]));
    assert!(s.can_pin_messages(sf(TEXT)));
    assert!(!s.can_manage_messages(sf(TEXT)));
    // Unknown member: fail open.
    let s = store(None);
    assert!(s.can_pin_messages(sf(TEXT)));
    assert!(s.can_manage_messages(sf(TEXT)));
    // DMs: anyone pins; nobody deletes the other person's messages.
    assert!(s.can_pin_messages(sf(DM)));
    assert!(!s.can_manage_messages(sf(DM)));
    // Unknown channel: no.
    assert!(!s.can_pin_messages(sf(999)));
    assert!(!s.can_manage_messages(sf(999)));
}

#[test]
fn mark_unread_moves_marker_back_and_recounts_mentions() {
    let mut s = store(Some(&[]));
    with_history(&mut s, TEXT);
    let (read, mentions) = s.mark_unread_from(sf(TEXT), sf(103));
    assert_eq!(read, sf(102), "the previous message stays read");
    assert_eq!(mentions, 1, "only 104 mentions me after 102");
    assert_eq!(s.last_read_message_id(sf(TEXT)), Some(sf(102)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 1);
    assert!(s.channel_unread(sf(TEXT)));
    assert_eq!(s.guild_mention_count(sf(G)), 1);

    // Further back: both mentions count again.
    let (read, mentions) = s.mark_unread_from(sf(TEXT), sf(101));
    assert_eq!((read, mentions), (sf(100), 2));
}

#[test]
fn mark_unread_on_first_known_message_uses_id_minus_one() {
    let mut s = store(Some(&[]));
    with_history(&mut s, TEXT);
    let (read, mentions) = s.mark_unread_from(sf(TEXT), sf(100));
    assert_eq!(read, sf(99));
    assert_eq!(mentions, 2);
}

#[test]
fn mark_unread_ignores_my_own_messages_and_counts_all_in_dms() {
    let mut s = store(Some(&[]));
    let msgs = vec![
        msg(200, DM, OTHER, false),
        msg(201, DM, OTHER, false),
        msg(202, DM, ME, false),
        msg(203, DM, OTHER, false),
    ];
    s.insert_history(sf(DM), msgs);
    s.mark_read(sf(DM));
    let (read, mentions) = s.mark_unread_from(sf(DM), sf(201));
    assert_eq!(read, sf(200));
    assert_eq!(mentions, 2, "201 and 203; my own 202 never counts");
    assert_eq!(s.dm_mention_count(), 2);
}

#[test]
fn later_incoming_message_keeps_channel_unread() {
    let mut s = store(Some(&[]));
    with_history(&mut s, TEXT);
    s.mark_unread_from(sf(TEXT), sf(105));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 0);
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(
        106, TEXT, OTHER, true,
    ))));
    assert!(s.channel_unread(sf(TEXT)));
    assert_eq!(s.last_read_message_id(sf(TEXT)), Some(sf(104)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 1);
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(
        107, TEXT, OTHER, false,
    ))));
    assert!(s.channel_unread(sf(TEXT)));
    // Reading it again clears everything.
    s.mark_read(sf(TEXT));
    assert!(!s.channel_unread(sf(TEXT)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 0);
}

#[test]
fn echoed_manual_ack_keeps_the_mention_count() {
    let mut s = store(Some(&[]));
    with_history(&mut s, TEXT);
    s.mark_unread_from(sf(TEXT), sf(103));
    // Discord echoes the manual ack as MESSAGE_ACK at the same id.
    s.apply(&GatewayEvent::MessageAck {
        channel_id: sf(TEXT),
        message_id: sf(102),
    });
    assert!(s.channel_unread(sf(TEXT)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 1);
    // A forward ack (read elsewhere) still clears.
    s.apply(&GatewayEvent::MessageAck {
        channel_id: sf(TEXT),
        message_id: sf(105),
    });
    assert!(!s.channel_unread(sf(TEXT)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 0);
}

#[test]
fn message_update_sets_and_keeps_pinned() {
    let mut s = store(Some(&[]));
    with_history(&mut s, TEXT);
    let pinned = |s: &Store| {
        s.messages(sf(TEXT))
            .iter()
            .find(|m| m.id == sf(103))
            .unwrap()
            .pinned
    };
    assert!(!pinned(&s));
    let update = |p: Option<bool>| {
        GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
            id: sf(103),
            channel_id: sf(TEXT),
            pinned: p,
            ..Default::default()
        }))
    };
    s.apply(&update(Some(true)));
    assert!(pinned(&s));
    s.apply(&update(None));
    assert!(pinned(&s), "an update without `pinned` keeps it");
    s.apply(&update(Some(false)));
    assert!(!pinned(&s));
}

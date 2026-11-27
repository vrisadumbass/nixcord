//! Composer `@` autocomplete helpers: who can be mentioned in a channel, and whether
//! `@everyone` / `@here` may be used. Invented data.

use super::*;
use nixcord_model::Ready;

const ME: u64 = 1;
const G: u64 = 100;
const TEXT: u64 = 10;
const THREAD: u64 = 11;
const DM: u64 = 20;
const GROUP: u64 = 21;
/// A role granting MENTION_EVERYONE.
const PINGER: u64 = 7;
const MENTION_EVERYONE: u64 = 1 << 17;

fn sf(n: u64) -> Snowflake {
    Snowflake(n)
}

fn user(id: u64, name: &str) -> User {
    User {
        id: sf(id),
        username: name.into(),
        ..Default::default()
    }
}

fn member(id: u64, name: &str, nick: Option<&str>, roles: &[u64]) -> GuildMember {
    GuildMember {
        user: Some(user(id, name)),
        roles: roles.iter().copied().map(sf).collect(),
        nick: nick.map(Into::into),
        ..Default::default()
    }
}

fn chan(id: u64, kind: ChannelKind, guild: Option<u64>, parent: Option<u64>) -> Channel {
    Channel {
        id: sf(id),
        kind,
        guild_id: guild.map(sf),
        parent_id: parent.map(sf),
        ..Default::default()
    }
}

fn msg(id: u64, channel: u64, author: u64, name: &str) -> Message {
    Message {
        id: sf(id),
        channel_id: sf(channel),
        author: user(author, name),
        content: "hello".into(),
        timestamp: "2024-01-01T00:00:00Z".into(),
        ..Default::default()
    }
}

/// Me (roles `my_roles`) in guild G with members 2 ("ada", nick "Captain") and 3 ("bo"),
/// a text channel and a thread; a DM with 4 and a group DM with 4 and 5.
fn store(my_roles: &[u64]) -> Store {
    let everyone = Role {
        id: sf(G),
        name: "@everyone".into(),
        permissions: perm::VIEW_CHANNEL | perm::SEND_MESSAGES,
        ..Default::default()
    };
    let pinger = Role {
        id: sf(PINGER),
        name: "pinger".into(),
        permissions: MENTION_EVERYONE,
        position: 1,
        ..Default::default()
    };
    let guild = Guild {
        id: sf(G),
        name: "g".into(),
        channels: vec![chan(TEXT, ChannelKind::Text, Some(G), None)],
        threads: vec![chan(THREAD, ChannelKind::PublicThread, Some(G), Some(TEXT))],
        roles: vec![everyone, pinger],
        members: vec![
            member(ME, "me", None, my_roles),
            member(2, "ada", Some("Captain"), &[]),
            member(3, "bo", None, &[]),
        ],
        ..Default::default()
    };
    let dm = Channel {
        recipients: vec![user(4, "cy")],
        ..chan(DM, ChannelKind::Dm, None, None)
    };
    let group = Channel {
        recipients: vec![user(4, "cy"), user(5, "dee")],
        ..chan(GROUP, ChannelKind::GroupDm, None, None)
    };
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(ME, "me"),
        guilds: vec![guild],
        private_channels: vec![dm, group],
        session_id: "s".into(),
        ..Default::default()
    })));
    s
}

fn ids(c: &[MentionCandidate<'_>]) -> Vec<u64> {
    c.iter().map(|c| c.user.id.0).collect()
}

#[test]
fn everyone_needs_the_permission() {
    let s = store(&[]);
    assert!(!s.can_mention_everyone(sf(TEXT)));
    assert!(
        !s.can_mention_everyone(sf(THREAD)),
        "threads use the parent"
    );
    let s = store(&[PINGER]);
    assert!(s.can_mention_everyone(sf(TEXT)));
    assert!(s.can_mention_everyone(sf(THREAD)));
}

#[test]
fn everyone_never_in_private_or_unknown_channels() {
    let s = store(&[PINGER]);
    assert!(!s.can_mention_everyone(sf(DM)));
    assert!(!s.can_mention_everyone(sf(GROUP)));
    assert!(!s.can_mention_everyone(sf(999)));
}

#[test]
fn everyone_fails_open_when_my_roles_are_unknown() {
    let mut s = store(&[]);
    // A server we know nothing about our member in: permissions unknown.
    s.apply(&GatewayEvent::GuildCreate(Box::new(Guild {
        id: sf(300),
        name: "other".into(),
        channels: vec![chan(301, ChannelKind::Text, Some(300), None)],
        ..Default::default()
    })));
    assert!(s.can_mention_everyone(sf(301)));
}

#[test]
fn guild_candidates_recent_authors_first_then_members() {
    let mut s = store(&[]);
    s.insert_history(
        sf(TEXT),
        vec![
            msg(50, TEXT, 3, "bo"),
            msg(51, TEXT, 9, "zed"),
            msg(52, TEXT, 3, "bo"),
        ],
    );
    let c = s.mention_candidates(sf(TEXT));
    // Newest author first, each once; then the cached members by id.
    assert_eq!(ids(&c), vec![3, 9, ME, 2]);
    assert_eq!(c[0].recent, Some(0));
    assert_eq!(c[1].recent, Some(1));
    assert_eq!(c[2].recent, None);
    let ada = c.iter().find(|c| c.user.id == sf(2)).unwrap();
    assert_eq!(ada.nick, Some("Captain"));
    assert_eq!(ada.user.username, "ada");
}

#[test]
fn candidates_stay_in_their_server() {
    let mut s = store(&[]);
    s.apply(&GatewayEvent::GuildCreate(Box::new(Guild {
        id: sf(300),
        name: "other".into(),
        channels: vec![chan(301, ChannelKind::Text, Some(300), None)],
        members: vec![member(8, "eve", None, &[])],
        ..Default::default()
    })));
    assert!(!ids(&s.mention_candidates(sf(TEXT))).contains(&8));
    assert_eq!(ids(&s.mention_candidates(sf(301))), vec![8]);
}

#[test]
fn private_candidates_are_the_recipients() {
    let s = store(&[]);
    assert_eq!(ids(&s.mention_candidates(sf(DM))), vec![4]);
    assert_eq!(ids(&s.mention_candidates(sf(GROUP))), vec![4, 5]);
    assert!(s.mention_candidates(sf(DM))[0].nick.is_none());
    assert!(s.mention_candidates(sf(999)).is_empty());
}

#[test]
fn thread_candidates_use_the_server_members() {
    let s = store(&[]);
    assert_eq!(ids(&s.mention_candidates(sf(THREAD))), vec![ME, 2, 3]);
}

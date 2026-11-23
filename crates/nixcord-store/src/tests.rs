use super::*;
use nixcord_model::{Attachment, ChannelKind, MessageUpdate, Ready};

fn sf(n: u64) -> Snowflake {
    Snowflake(n)
}

fn user(id: u64) -> User {
    User {
        id: sf(id),
        username: format!("user{id}"),
        global_name: None,
        avatar: None,
        bot: false,
    }
}

fn chan(id: u64, kind: ChannelKind, guild: Option<u64>, parent: Option<u64>, pos: i32) -> Channel {
    Channel {
        id: sf(id),
        kind,
        guild_id: guild.map(sf),
        name: Some(format!("c{id}")),
        topic: None,
        position: pos,
        parent_id: parent.map(sf),
        recipients: Vec::new(),
        last_message_id: None,
    }
}

fn dm(id: u64, last: Option<u64>, recipient: u64) -> Channel {
    Channel {
        recipients: vec![user(recipient)],
        last_message_id: last.map(sf),
        name: None,
        ..chan(id, ChannelKind::Dm, None, None, 0)
    }
}

fn guild(id: u64, channels: Vec<Channel>) -> Guild {
    Guild {
        id: sf(id),
        name: format!("g{id}"),
        icon: None,
        channels,
        roles: Vec::new(),
        unavailable: false,
    }
}

fn msg(id: u64, channel: u64, author: u64) -> Message {
    Message {
        id: sf(id),
        channel_id: sf(channel),
        guild_id: None,
        author: user(author),
        content: format!("m{id}"),
        timestamp: "2024-01-01T00:00:00Z".into(),
        edited_timestamp: None,
        attachments: Vec::new(),
        mentions: Vec::new(),
        referenced_message: None,
        ..Default::default()
    }
}

fn ready(me: u64, guilds: Vec<Guild>, private: Vec<Channel>) -> GatewayEvent {
    GatewayEvent::Ready(Box::new(Ready {
        user: user(me),
        guilds,
        private_channels: private,
        session_id: "s".into(),
        resume_gateway_url: None,
    }))
}

fn ids(msgs: &[Message]) -> Vec<u64> {
    msgs.iter().map(|m| m.id.0).collect()
}

fn chan_ids(chs: &[&Channel]) -> Vec<u64> {
    chs.iter().map(|c| c.id.0).collect()
}

/// Store with me=1, guild 100 with text channel 10, and a DM 20.
fn basic() -> Store {
    let mut s = Store::new();
    s.apply(&ready(
        1,
        vec![guild(100, vec![chan(10, ChannelKind::Text, None, None, 0)])],
        vec![dm(20, None, 2)],
    ));
    s
}

#[test]
fn new_and_default() {
    let s = Store::default();
    assert_eq!(s.connection(), ConnectionState::Offline);
    assert!(s.me().is_none());
    assert!(s.guilds().is_empty());
    assert!(s.messages(sf(1)).is_empty());
    assert!(!s.history_loaded(sf(1)));
    let _ = Store::new();
}

#[test]
fn connection_lifecycle() {
    let mut s = Store::new();
    s.apply(&GatewayEvent::Connecting);
    assert_eq!(s.connection(), ConnectionState::Connecting);
    s.apply(&ready(1, vec![], vec![]));
    assert_eq!(s.connection(), ConnectionState::Connected);
    s.apply(&GatewayEvent::Disconnected {
        reason: "x".into(),
        will_retry: true,
    });
    assert_eq!(s.connection(), ConnectionState::Reconnecting);
    s.apply(&GatewayEvent::Resumed);
    assert_eq!(s.connection(), ConnectionState::Connected);
    s.apply(&GatewayEvent::Disconnected {
        reason: "x".into(),
        will_retry: false,
    });
    assert_eq!(s.connection(), ConnectionState::Offline);
    s.apply(&GatewayEvent::Connecting);
    s.apply(&GatewayEvent::AuthFailed);
    assert_eq!(s.connection(), ConnectionState::Offline);
}

#[test]
fn ready_populates_and_attaches_guild_id() {
    let s = basic();
    assert_eq!(s.me().unwrap().id, sf(1));
    assert_eq!(s.user(sf(1)).unwrap().username, "user1");
    assert_eq!(s.user(sf(2)).unwrap().username, "user2"); // DM recipient cached
    assert_eq!(s.guilds().len(), 1);
    assert_eq!(s.channel(sf(10)).unwrap().guild_id, Some(sf(100)));
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10]);
    assert_eq!(chan_ids(&s.private_channels()), vec![20]);
    assert!(s.guild(sf(100)).is_some());
    assert!(s.guild_channels(sf(999)).is_empty());
}

#[test]
fn ready_resets_state() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&GatewayEvent::TypingStart {
        channel_id: sf(10),
        user_id: sf(3),
    });
    s.apply(&ready(7, vec![guild(200, vec![])], vec![]));
    assert_eq!(s.me().unwrap().id, sf(7));
    assert!(s.guild(sf(100)).is_none());
    assert!(s.channel(sf(10)).is_none());
    assert!(s.channel(sf(20)).is_none());
    assert!(s.messages(sf(10)).is_empty());
    assert!(!s.history_loaded(sf(10)));
    assert!(s.typing(sf(10), Instant::now()).is_empty());
    assert!(s.user(sf(3)).is_none());
    assert_eq!(
        s.guilds().iter().map(|g| g.id.0).collect::<Vec<_>>(),
        vec![200]
    );
}

#[test]
fn guild_order_preserved() {
    let mut s = Store::new();
    s.apply(&ready(1, vec![guild(3, vec![]), guild(1, vec![])], vec![]));
    s.apply(&GatewayEvent::GuildCreate(Box::new(guild(2, vec![]))));
    // Re-create of an existing guild keeps its slot.
    s.apply(&GatewayEvent::GuildCreate(Box::new(guild(3, vec![]))));
    let order: Vec<u64> = s.guilds().iter().map(|g| g.id.0).collect();
    assert_eq!(order, vec![3, 1, 2]);
}

#[test]
fn guild_update_preserves_channels_when_absent() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    let mut upd = guild(100, vec![]);
    upd.name = "renamed".into();
    s.apply(&GatewayEvent::GuildUpdate(Box::new(upd)));
    assert_eq!(s.guild(sf(100)).unwrap().name, "renamed");
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10]);
    assert_eq!(ids(s.messages(sf(10))), vec![5]);
}

#[test]
fn guild_update_with_channels_replaces_them() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&GatewayEvent::GuildUpdate(Box::new(guild(
        100,
        vec![chan(11, ChannelKind::Text, None, None, 0)],
    ))));
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![11]);
    assert!(s.channel(sf(10)).is_none());
    assert!(s.messages(sf(10)).is_empty());
}

#[test]
fn guild_create_fills_unavailable_guild() {
    let mut s = Store::new();
    let mut g = guild(5, vec![]);
    g.name.clear();
    g.unavailable = true;
    s.apply(&ready(1, vec![g], vec![]));
    assert!(s.guild(sf(5)).unwrap().unavailable);
    s.apply(&GatewayEvent::GuildCreate(Box::new(guild(
        5,
        vec![chan(50, ChannelKind::Text, None, None, 0)],
    ))));
    assert!(!s.guild(sf(5)).unwrap().unavailable);
    assert_eq!(s.guild(sf(5)).unwrap().name, "g5");
    assert_eq!(chan_ids(&s.guild_channels(sf(5))), vec![50]);
}

#[test]
fn guild_delete_drops_channels_and_messages() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(100),
        unavailable: false,
    });
    assert!(s.guild(sf(100)).is_none());
    assert!(s.guilds().is_empty());
    assert!(s.channel(sf(10)).is_none());
    assert!(s.messages(sf(10)).is_empty());
    assert!(!s.history_loaded(sf(10)));
    assert!(s.guild_channels(sf(100)).is_empty());
    // DMs untouched.
    assert!(s.channel(sf(20)).is_some());
}

#[test]
fn channel_sort_with_categories() {
    use ChannelKind::*;
    let g = 100;
    let channels = vec![
        chan(1, Category, None, None, 1),  // cat B (pos 1)
        chan(2, Category, None, None, 0),  // cat A (pos 0)
        chan(3, Text, None, Some(1), 0),   // B child
        chan(4, Text, None, Some(2), 1),   // A child pos 1
        chan(5, Voice, None, Some(2), 0),  // A child pos 0
        chan(6, Text, None, None, 5),      // uncategorized pos 5
        chan(7, Text, None, None, 0),      // uncategorized pos 0
        chan(8, Text, None, Some(999), 0), // unknown parent -> uncategorized
        chan(9, Text, None, Some(2), 1),   // A child pos 1, id tie-break after 4
        chan(12, Category, None, None, 1), // cat C, same pos as B, higher id
    ];
    let mut s = Store::new();
    s.apply(&ready(1, vec![guild(g, channels)], vec![]));
    assert_eq!(
        chan_ids(&s.guild_channels(sf(g))),
        vec![7, 8, 6, 2, 5, 4, 9, 1, 3, 12]
    );
}

#[test]
fn channel_create_update_delete() {
    let mut s = basic();
    s.apply(&GatewayEvent::ChannelCreate(Box::new(chan(
        11,
        ChannelKind::Text,
        Some(100),
        None,
        1,
    ))));
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10, 11]);

    // Update moves position; no duplicate entries.
    s.apply(&GatewayEvent::ChannelUpdate(Box::new(chan(
        11,
        ChannelKind::Text,
        Some(100),
        None,
        -1,
    ))));
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![11, 10]);

    // DM create goes to private channels.
    s.apply(&GatewayEvent::ChannelCreate(Box::new(dm(21, Some(50), 9))));
    assert_eq!(chan_ids(&s.private_channels()), vec![21, 20]);
    assert!(s.user(sf(9)).is_some());

    s.insert_history(sf(11), vec![msg(60, 11, 3)]);
    s.apply(&GatewayEvent::ChannelDelete(Box::new(chan(
        11,
        ChannelKind::Text,
        Some(100),
        None,
        0,
    ))));
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10]);
    assert!(s.channel(sf(11)).is_none());
    assert!(s.messages(sf(11)).is_empty());

    s.apply(&GatewayEvent::ChannelDelete(Box::new(dm(21, None, 9))));
    assert_eq!(chan_ids(&s.private_channels()), vec![20]);
}

#[test]
fn channel_update_keeps_newer_last_message_id() {
    let mut s = basic();
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(500, 20, 2))));
    s.apply(&GatewayEvent::ChannelUpdate(Box::new(dm(20, Some(400), 2))));
    assert_eq!(s.channel(sf(20)).unwrap().last_message_id, Some(sf(500)));
}

#[test]
fn dm_sort_by_last_message() {
    let mut s = Store::new();
    s.apply(&ready(
        1,
        vec![],
        vec![
            dm(1, Some(10), 2),
            dm(2, None, 3),
            dm(3, Some(30), 4),
            dm(4, Some(20), 5),
            dm(5, None, 6),
        ],
    ));
    assert_eq!(chan_ids(&s.private_channels()), vec![3, 4, 1, 5, 2]);
    // New message bumps channel 1 to the top.
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(40, 1, 2))));
    assert_eq!(chan_ids(&s.private_channels()), vec![1, 3, 4, 5, 2]);
}

#[test]
fn message_create_only_stored_when_loaded_or_nonempty() {
    let mut s = basic();
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(5, 10, 3))));
    assert!(s.messages(sf(10)).is_empty());
    // last_message_id bumped and author cached anyway.
    assert_eq!(s.channel(sf(10)).unwrap().last_message_id, Some(sf(5)));
    assert_eq!(s.user(sf(3)).unwrap().username, "user3");

    // Empty history load still enables live appends.
    s.insert_history(sf(10), vec![]);
    assert!(s.history_loaded(sf(10)));
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(6, 10, 3))));
    assert_eq!(ids(s.messages(sf(10))), vec![6]);

    // Older bump does not regress last_message_id.
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(4, 10, 3))));
    assert_eq!(s.channel(sf(10)).unwrap().last_message_id, Some(sf(6)));
    assert_eq!(ids(s.messages(sf(10))), vec![4, 6]);
}

#[test]
fn message_create_ordering_and_dedupe() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(10, 10, 3), msg(30, 10, 3)]);
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(40, 10, 3))));
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(20, 10, 3)))); // out of order
    let mut dup = msg(30, 10, 3);
    dup.content = "replaced".into();
    s.apply(&GatewayEvent::MessageCreate(Box::new(dup))); // duplicate
    assert_eq!(ids(s.messages(sf(10))), vec![10, 20, 30, 40]);
    assert_eq!(s.messages(sf(10))[2].content, "replaced");
}

#[test]
fn mentions_cached() {
    let mut s = basic();
    let mut m = msg(5, 10, 3);
    m.mentions = vec![user(42)];
    s.insert_history(sf(10), vec![m]);
    assert!(s.user(sf(42)).is_some());
    assert!(s.user(sf(3)).is_some());
}

#[test]
fn history_dedupe_on_overlap_and_sort() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3), msg(3, 10, 3), msg(4, 10, 3)]);
    assert_eq!(ids(s.messages(sf(10))), vec![3, 4, 5]);

    // Live edit on 4, then an overlapping older page that includes stale 4.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(4),
        channel_id: sf(10),
        content: Some("edited".into()),
        edited_timestamp: None,
        attachments: None,
        ..Default::default()
    })));
    s.insert_history(
        sf(10),
        vec![msg(4, 10, 3), msg(2, 10, 3), msg(1, 10, 3), msg(3, 10, 3)],
    );
    assert_eq!(ids(s.messages(sf(10))), vec![1, 2, 3, 4, 5]);
    assert_eq!(s.messages(sf(10))[3].content, "edited");
    // Duplicates within the incoming batch itself.
    s.insert_history(sf(10), vec![msg(6, 10, 3), msg(6, 10, 3)]);
    assert_eq!(ids(s.messages(sf(10))), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(s.channel(sf(10)).unwrap().last_message_id, Some(sf(6)));
}

#[test]
fn cap_eviction_on_history_keeps_newest() {
    let mut s = basic();
    let n = MAX_MESSAGES_PER_CHANNEL as u64 + 50;
    // Reverse order to exercise sorting.
    s.insert_history(sf(10), (1..=n).rev().map(|i| msg(i, 10, 3)).collect());
    let m = s.messages(sf(10));
    assert_eq!(m.len(), MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(m.first().unwrap().id, sf(51));
    assert_eq!(m.last().unwrap().id, sf(n));
}

#[test]
fn cap_eviction_on_live_append() {
    let mut s = basic();
    let max = MAX_MESSAGES_PER_CHANNEL as u64;
    s.insert_history(sf(10), (1..=max).map(|i| msg(i, 10, 3)).collect());
    assert_eq!(s.messages(sf(10)).len(), MAX_MESSAGES_PER_CHANNEL);
    for i in max + 1..=max + 3 {
        s.apply(&GatewayEvent::MessageCreate(Box::new(msg(i, 10, 3))));
    }
    let m = s.messages(sf(10));
    assert_eq!(m.len(), MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(m.first().unwrap().id, sf(4));
    assert_eq!(m.last().unwrap().id, sf(max + 3));
    assert!(m.windows(2).all(|w| w[0].id < w[1].id));
}

#[test]
fn message_update_partial_patch() {
    let mut s = basic();
    let mut original = msg(5, 10, 3);
    original.attachments = vec![Attachment {
        id: sf(77),
        filename: "a.png".into(),
        url: "https://x/a.png".into(),
        size: 1,
        content_type: None,
        width: None,
        height: None,
    }];
    s.insert_history(sf(10), vec![original]);

    // Only edited_timestamp present: content and attachments untouched.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(5),
        channel_id: sf(10),
        content: None,
        edited_timestamp: Some("2024-01-02T00:00:00Z".into()),
        attachments: None,
        ..Default::default()
    })));
    let m = &s.messages(sf(10))[0];
    assert_eq!(m.content, "m5");
    assert_eq!(m.attachments.len(), 1);
    assert_eq!(m.edited_timestamp.as_deref(), Some("2024-01-02T00:00:00Z"));

    // Content + empty attachments.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(5),
        channel_id: sf(10),
        content: Some("new".into()),
        edited_timestamp: None,
        attachments: Some(vec![]),
        ..Default::default()
    })));
    let m = &s.messages(sf(10))[0];
    assert_eq!(m.content, "new");
    assert!(m.attachments.is_empty());
    assert_eq!(m.edited_timestamp.as_deref(), Some("2024-01-02T00:00:00Z"));

    // Unknown message / channel: no-op, no panic.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(999),
        channel_id: sf(12345),
        content: Some("x".into()),
        edited_timestamp: None,
        attachments: None,
        ..Default::default()
    })));
}

#[test]
fn message_delete() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(1, 10, 3), msg(2, 10, 3), msg(3, 10, 3)]);
    s.apply(&GatewayEvent::MessageDelete {
        id: sf(2),
        channel_id: sf(10),
    });
    assert_eq!(ids(s.messages(sf(10))), vec![1, 3]);
    // Missing ids are ignored.
    s.apply(&GatewayEvent::MessageDelete {
        id: sf(2),
        channel_id: sf(10),
    });
    s.apply(&GatewayEvent::MessageDelete {
        id: sf(1),
        channel_id: sf(999),
    });
    assert_eq!(ids(s.messages(sf(10))), vec![1, 3]);
}

#[test]
fn typing_expiry_and_excludes_me() {
    let mut s = basic();
    s.apply(&GatewayEvent::TypingStart {
        channel_id: sf(10),
        user_id: sf(3),
    });
    s.apply(&GatewayEvent::TypingStart {
        channel_id: sf(10),
        user_id: sf(1), // me
    });
    s.apply(&GatewayEvent::TypingStart {
        channel_id: sf(10),
        user_id: sf(3), // repeat: no duplicate
    });
    let now = Instant::now();
    assert_eq!(s.typing(sf(10), now), vec![sf(3)]);
    assert_eq!(s.typing(sf(10), now + Duration::from_secs(9)), vec![sf(3)]);
    assert!(s.typing(sf(10), now + Duration::from_secs(11)).is_empty());
    assert!(s.typing(sf(11), now).is_empty());
}

#[test]
fn message_clears_typing_for_author() {
    let mut s = basic();
    for u in [3, 4] {
        s.apply(&GatewayEvent::TypingStart {
            channel_id: sf(10),
            user_id: sf(u),
        });
    }
    s.apply(&GatewayEvent::TypingStart {
        channel_id: sf(20),
        user_id: sf(3),
    });
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(5, 10, 3))));
    let now = Instant::now();
    assert_eq!(s.typing(sf(10), now), vec![sf(4)]);
    // Other channel unaffected.
    assert_eq!(s.typing(sf(20), now), vec![sf(3)]);
}

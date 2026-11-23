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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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

// ---- reactions ----------------------------------------------------------------------

fn uni(name: &str) -> nixcord_model::Emoji {
    nixcord_model::Emoji {
        id: None,
        name: Some(name.into()),
        animated: false,
    }
}

fn custom(id: u64, name: &str) -> nixcord_model::Emoji {
    nixcord_model::Emoji {
        id: Some(sf(id)),
        name: Some(name.into()),
        animated: false,
    }
}

fn react(add: bool, msg_id: u64, user: u64, emoji: nixcord_model::Emoji) -> GatewayEvent {
    if add {
        GatewayEvent::ReactionAdd {
            channel_id: sf(10),
            message_id: sf(msg_id),
            user_id: sf(user),
            emoji,
        }
    } else {
        GatewayEvent::ReactionRemove {
            channel_id: sf(10),
            message_id: sf(msg_id),
            user_id: sf(user),
            emoji,
        }
    }
}

fn reactions(s: &Store, id: u64) -> Vec<(Option<String>, u32, bool)> {
    s.messages(sf(10))
        .iter()
        .find(|m| m.id == sf(id))
        .unwrap()
        .reactions
        .iter()
        .map(|r| (r.emoji.name.clone(), r.count, r.me))
        .collect()
}

#[test]
fn reaction_add_and_remove_counts() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&react(true, 5, 3, uni("a")));
    s.apply(&react(true, 5, 4, uni("a")));
    s.apply(&react(true, 5, 3, uni("b")));
    assert_eq!(
        reactions(&s, 5),
        vec![(Some("a".into()), 2, false), (Some("b".into()), 1, false)]
    );
    s.apply(&react(false, 5, 3, uni("a")));
    assert_eq!(reactions(&s, 5)[0], (Some("a".into()), 1, false));
    // Count 0 removes the reaction entirely.
    s.apply(&react(false, 5, 3, uni("b")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 1, false)]);
    // Removing a reaction that does not exist is a no-op.
    s.apply(&react(false, 5, 3, uni("zzz")));
    assert_eq!(reactions(&s, 5).len(), 1);
    // Unknown message / channel: ignored, no panic.
    s.apply(&react(true, 999, 3, uni("a")));
    s.apply(&GatewayEvent::ReactionAdd {
        channel_id: sf(12345),
        message_id: sf(5),
        user_id: sf(3),
        emoji: uni("a"),
    });
}

#[test]
fn reaction_me_flag_and_idempotence() {
    let mut s = basic(); // me = 1
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&react(true, 5, 1, uni("a")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 1, true)]);
    // Echo of our own add (optimistic + gateway) does not double count.
    s.apply(&react(true, 5, 1, uni("a")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 1, true)]);
    s.apply(&react(true, 5, 2, uni("a")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 2, true)]);
    s.apply(&react(false, 5, 1, uni("a")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 1, false)]);
    // Duplicate removal of ours is a no-op.
    s.apply(&react(false, 5, 1, uni("a")));
    assert_eq!(reactions(&s, 5), vec![(Some("a".into()), 1, false)]);
}

#[test]
fn custom_emoji_reactions_match_by_id() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&react(true, 5, 3, custom(77, "blob")));
    // Same id, renamed emoji: same reaction.
    s.apply(&react(true, 5, 4, custom(77, "blob_renamed")));
    // Unicode with the same name is a different reaction.
    s.apply(&react(true, 5, 4, uni("blob")));
    assert_eq!(
        reactions(&s, 5),
        vec![
            (Some("blob".into()), 2, false),
            (Some("blob".into()), 1, false)
        ]
    );
    s.apply(&react(false, 5, 3, custom(77, "x")));
    s.apply(&react(false, 5, 4, custom(77, "x")));
    assert_eq!(reactions(&s, 5), vec![(Some("blob".into()), 1, false)]);
}

// ---- members, colors, nicks ---------------------------------------------------------

fn role(id: u64, color: u32, position: i32) -> nixcord_model::Role {
    nixcord_model::Role {
        id: sf(id),
        name: format!("r{id}"),
        color,
        position,
        ..Default::default()
    }
}

fn member_msg(id: u64, author: u64, roles: &[u64], nick: Option<&str>) -> Message {
    Message {
        member: Some(nixcord_model::PartialMember {
            roles: roles.iter().copied().map(sf).collect(),
            nick: nick.map(Into::into),
            avatar: None,
        }),
        ..msg(id, 10, author)
    }
}

/// me=1, guild 100 (channel 10) with roles: 500 red pos 1, 501 green pos 5,
/// 502 uncolored pos 9, 503 blue pos 5 (ties 501 on position, higher id).
fn with_roles() -> Store {
    let mut g = guild(100, vec![chan(10, ChannelKind::Text, None, None, 0)]);
    g.roles = vec![
        role(500, 0xFF0000, 1),
        role(501, 0x00FF00, 5),
        role(502, 0, 9),
        role(503, 0x0000FF, 5),
    ];
    let mut s = Store::new();
    s.apply(&ready(1, vec![g], vec![]));
    s
}

#[test]
fn member_color_highest_colored_role() {
    let mut s = with_roles();
    // REST history: no guild_id on the message; the guild comes from the channel.
    s.insert_history(
        sf(10),
        vec![
            member_msg(1, 3, &[500, 502], Some("Nick")),
            member_msg(2, 4, &[502], None),
            member_msg(3, 5, &[500, 501, 503], Some("")),
        ],
    );
    // 502 is higher but uncolored, so 500 wins.
    assert_eq!(s.member_color(sf(100), sf(3)), Some(0xFF0000));
    // Only uncolored roles.
    assert_eq!(s.member_color(sf(100), sf(4)), None);
    // Tie on position 5: the lower id (501) ranks higher.
    assert_eq!(s.member_color(sf(100), sf(5)), Some(0x00FF00));
    // Unknown member / guild.
    assert_eq!(s.member_color(sf(100), sf(99)), None);
    assert_eq!(s.member_color(sf(999), sf(3)), None);

    assert_eq!(s.member_nick(sf(100), sf(3)), Some("Nick"));
    assert_eq!(s.member_nick(sf(100), sf(4)), None);
    assert_eq!(s.member_nick(sf(100), sf(5)), None, "empty nick ignored");

    // A live message updates the cached member.
    let mut m = member_msg(10, 3, &[501], Some("New"));
    m.guild_id = Some(sf(100));
    s.apply(&GatewayEvent::MessageCreate(Box::new(m)));
    assert_eq!(s.member_color(sf(100), sf(3)), Some(0x00FF00));
    assert_eq!(s.member_nick(sf(100), sf(3)), Some("New"));
}

#[test]
fn members_dropped_with_guild_but_not_on_outage() {
    let mut s = with_roles();
    s.insert_history(sf(10), vec![member_msg(1, 3, &[500], Some("N"))]);
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(100),
        unavailable: true,
    });
    assert_eq!(s.member_nick(sf(100), sf(3)), Some("N"));
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(100),
        unavailable: false,
    });
    assert_eq!(s.member_nick(sf(100), sf(3)), None);
}

// ---- guild outage -------------------------------------------------------------------

#[test]
fn guild_delete_unavailable_keeps_guild_then_create_restores() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(5, 10, 3)]);
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(100),
        unavailable: true,
    });
    let g = s.guild(sf(100)).unwrap();
    assert!(g.unavailable);
    assert_eq!(g.name, "g100");
    assert_eq!(s.guilds().len(), 1);
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10]);
    assert_eq!(ids(s.messages(sf(10))), vec![5]);
    assert!(s.history_loaded(sf(10)));

    s.apply(&GatewayEvent::GuildCreate(Box::new(guild(
        100,
        vec![chan(10, ChannelKind::Text, None, None, 0)],
    ))));
    assert!(!s.guild(sf(100)).unwrap().unavailable);
    assert_eq!(s.guilds().len(), 1, "not duplicated in order");
    assert_eq!(
        ids(s.messages(sf(10))),
        vec![5],
        "same channel keeps messages"
    );

    // Outage for an unknown guild is ignored.
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(4242),
        unavailable: true,
    });
    assert!(s.guild(sf(4242)).is_none());
}

// ---- message update: embeds / mentions ----------------------------------------------

#[test]
fn message_update_embeds_and_mentions() {
    let mut s = basic();
    let mut original = msg(5, 10, 3);
    original.embeds = vec![nixcord_model::Embed {
        title: Some("old".into()),
        ..Default::default()
    }];
    original.mentions = vec![user(4)];
    s.insert_history(sf(10), vec![original]);

    // Absent fields leave existing ones alone.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(5),
        channel_id: sf(10),
        content: Some("edited".into()),
        ..Default::default()
    })));
    let m = &s.messages(sf(10))[0];
    assert_eq!(m.embeds[0].title.as_deref(), Some("old"));
    assert_eq!(m.mentions.len(), 1);

    // Embed unfurl after send: embeds replaced; new mentions cached as users.
    let mut new_user = user(8);
    new_user.global_name = Some("Eight".into());
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(5),
        channel_id: sf(10),
        embeds: Some(vec![nixcord_model::Embed {
            title: Some("new".into()),
            ..Default::default()
        }]),
        mentions: Some(vec![new_user]),
        ..Default::default()
    })));
    let m = &s.messages(sf(10))[0];
    assert_eq!(m.content, "edited");
    assert_eq!(m.embeds.len(), 1);
    assert_eq!(m.embeds[0].title.as_deref(), Some("new"));
    assert_eq!(
        m.mentions.iter().map(|u| u.id.0).collect::<Vec<_>>(),
        vec![8]
    );
    assert_eq!(s.user(sf(8)).unwrap().display_name(), "Eight");

    // Empty embeds clears them.
    s.apply(&GatewayEvent::MessageUpdate(Box::new(MessageUpdate {
        id: sf(5),
        channel_id: sf(10),
        embeds: Some(vec![]),
        ..Default::default()
    })));
    assert!(s.messages(sf(10))[0].embeds.is_empty());
}

// ---- mentions -----------------------------------------------------------------------

#[test]
fn is_mentioned_direct_everyone_and_roles() {
    let mut s = with_roles(); // me = 1
    let plain = msg(5, 10, 3);
    assert!(!s.is_mentioned(&plain));

    let mut direct = msg(6, 10, 3);
    direct.mentions = vec![user(4), user(1)];
    assert!(s.is_mentioned(&direct));

    let mut other = msg(7, 10, 3);
    other.mentions = vec![user(4)];
    assert!(!s.is_mentioned(&other));

    let mut everyone = msg(8, 10, 3);
    everyone.mention_everyone = true;
    assert!(s.is_mentioned(&everyone));

    let mut role_ping = msg(9, 10, 3);
    role_ping.mention_roles = vec![sf(501)];
    // My roles are unknown until I have posted in the guild.
    assert!(!s.is_mentioned(&role_ping));
    s.insert_history(sf(10), vec![member_msg(2, 1, &[500, 501], None)]);
    assert!(s.is_mentioned(&role_ping));
    role_ping.mention_roles = vec![sf(503)];
    assert!(!s.is_mentioned(&role_ping));

    // No `me` (before READY): never mentioned.
    let s = Store::new();
    assert!(!s.is_mentioned(&direct));
}

// ---- sliding window / detached ------------------------------------------------------

#[test]
fn older_history_slides_window_and_detaches() {
    let mut s = basic();
    let max = MAX_MESSAGES_PER_CHANNEL as u64;
    // Newest page fills the cap: ids 1001..=1000+max.
    s.insert_history(sf(10), (1001..=1000 + max).map(|i| msg(i, 10, 3)).collect());
    assert!(!s.is_detached(sf(10)));

    // An older page of 50: the newest 50 are evicted.
    s.insert_older_history(sf(10), (951..=1000).rev().map(|i| msg(i, 10, 3)).collect());
    let got = ids(s.messages(sf(10)));
    assert_eq!(got.len(), MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(got[0], 951);
    assert_eq!(*got.last().unwrap(), 950 + max);
    assert!(got.windows(2).all(|w| w[0] < w[1]));
    assert!(s.is_detached(sf(10)));

    // Live messages are not appended while detached (but still bump last_message_id).
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(5000, 10, 3))));
    assert_eq!(*ids(s.messages(sf(10))).last().unwrap(), 950 + max);
    assert_eq!(s.channel(sf(10)).unwrap().last_message_id, Some(sf(5000)));

    // Deletes still apply to what is in the window.
    s.apply(&GatewayEvent::MessageDelete {
        id: sf(951),
        channel_id: sf(10),
    });
    assert_eq!(ids(s.messages(sf(10)))[0], 952);

    // Jump to present: window dropped, history unloaded, re-attached.
    s.jump_to_present(sf(10));
    assert!(!s.is_detached(sf(10)));
    assert!(!s.history_loaded(sf(10)));
    assert!(s.messages(sf(10)).is_empty());
    s.insert_history(sf(10), vec![msg(4999, 10, 3), msg(5000, 10, 3)]);
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(5001, 10, 3))));
    assert_eq!(ids(s.messages(sf(10))), vec![4999, 5000, 5001]);
}

#[test]
fn older_history_under_cap_does_not_detach() {
    let mut s = basic();
    s.insert_history(sf(10), vec![msg(100, 10, 3), msg(101, 10, 3)]);
    s.insert_older_history(
        sf(10),
        vec![msg(99, 10, 3), msg(98, 10, 3), msg(100, 10, 3)],
    );
    assert_eq!(ids(s.messages(sf(10))), vec![98, 99, 100, 101]);
    assert!(!s.is_detached(sf(10)));
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(102, 10, 3))));
    assert_eq!(*ids(s.messages(sf(10))).last().unwrap(), 102);
}

#[test]
fn newest_page_while_detached_reattaches() {
    let mut s = basic();
    let max = MAX_MESSAGES_PER_CHANNEL as u64;
    s.insert_history(sf(10), (1001..=1000 + max).map(|i| msg(i, 10, 3)).collect());
    s.insert_older_history(sf(10), vec![msg(1, 10, 3)]);
    assert!(s.is_detached(sf(10)));
    // A fresh newest page replaces the detached window instead of merging into it.
    s.insert_history(sf(10), vec![msg(9000, 10, 3), msg(9001, 10, 3)]);
    assert!(!s.is_detached(sf(10)));
    assert_eq!(ids(s.messages(sf(10))), vec![9000, 9001]);
}

#[test]
fn detached_state_cleared_with_channel_and_ready() {
    let mut s = basic();
    let max = MAX_MESSAGES_PER_CHANNEL as u64;
    s.insert_history(sf(10), (1001..=1000 + max).map(|i| msg(i, 10, 3)).collect());
    s.insert_older_history(sf(10), vec![msg(1, 10, 3)]);
    assert!(s.is_detached(sf(10)));
    s.apply(&GatewayEvent::ChannelDelete(Box::new(chan(
        10,
        ChannelKind::Text,
        Some(100),
        None,
        0,
    ))));
    assert!(!s.is_detached(sf(10)));

    let mut s = basic();
    s.insert_history(sf(10), (1001..=1000 + max).map(|i| msg(i, 10, 3)).collect());
    s.insert_older_history(sf(10), vec![msg(1, 10, 3)]);
    s.apply(&ready(1, vec![], vec![]));
    assert!(!s.is_detached(sf(10)));
}

#[test]
fn member_top_role_ignores_color() {
    let mut s = with_roles();
    s.insert_history(
        sf(10),
        vec![
            member_msg(1, 3, &[500, 502], None),
            member_msg(2, 5, &[501, 503], None),
        ],
    );
    // 502 is uncolored but has the highest position.
    assert_eq!(
        s.member_top_role(sf(100), sf(3)).map(|r| r.id),
        Some(sf(502))
    );
    // Tie on position 5: the lower id wins.
    assert_eq!(
        s.member_top_role(sf(100), sf(5)).map(|r| r.id),
        Some(sf(501))
    );
    assert!(s.member_top_role(sf(100), sf(99)).is_none());
}

// ---- presence, relationships --------------------------------------------------------

use nixcord_model::{
    ListMember, MemberListGroup, MemberListItem, MemberListOp, MemberListUpdate, Overwrite,
    OverwriteKind, Presence, ReadySupplemental, Relationship, RelationshipKind, Status,
};

fn presence(user: u64, status: Status) -> Presence {
    Presence {
        user_id: sf(user),
        status,
        ..Default::default()
    }
}

#[test]
fn presences_from_ready_updates_and_offline_drop() {
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(1),
        session_id: "s".into(),
        presences: vec![presence(2, Status::Online)],
        relationships: vec![
            Relationship {
                id: sf(2),
                kind: RelationshipKind::Friend,
                user: Some(user(2)),
                nickname: None,
            },
            Relationship {
                id: sf(3),
                kind: RelationshipKind::Blocked,
                user: Some(user(3)),
                nickname: None,
            },
        ],
        status: Some(Status::Dnd),
        ..Default::default()
    })));
    assert_eq!(s.status(sf(2)), Status::Online);
    assert_eq!(s.status(sf(99)), Status::Offline);
    assert_eq!(s.my_status(), Status::Dnd);
    assert_eq!(s.status(sf(1)), Status::Dnd, "own status from settings");
    let friends: Vec<u64> = s.friends().iter().map(|u| u.id.0).collect();
    assert_eq!(friends, vec![2]);

    s.apply(&GatewayEvent::PresenceUpdate {
        presence: Box::new(presence(2, Status::Idle)),
        user: None,
    });
    assert_eq!(s.status(sf(2)), Status::Idle);
    s.apply(&GatewayEvent::PresenceUpdate {
        presence: Box::new(presence(2, Status::Offline)),
        user: None,
    });
    assert!(
        s.presence(sf(2)).is_none(),
        "offline without activity not stored"
    );

    s.apply(&GatewayEvent::ReadySupplemental(Box::new(
        ReadySupplemental {
            presences: vec![presence(4, Status::Dnd)],
        },
    )));
    assert_eq!(s.status(sf(4)), Status::Dnd);
    s.apply(&GatewayEvent::OwnStatus(Status::Invisible));
    assert_eq!(
        s.status(sf(1)),
        Status::Offline,
        "invisible shows as offline"
    );
    s.apply(&GatewayEvent::RelationshipRemove { user_id: sf(2) });
    assert!(s.friends().is_empty());
}

// ---- member list ---------------------------------------------------------------------

fn group(id: &str, count: u32) -> MemberListItem {
    MemberListItem::Group(MemberListGroup {
        id: id.into(),
        count,
    })
}

fn member_item(id: u64, status: Option<Status>) -> MemberListItem {
    MemberListItem::Member(Box::new(ListMember {
        member: nixcord_model::GuildMember {
            user: Some(user(id)),
            roles: vec![sf(500)],
            nick: (id == 11).then(|| "Nick".to_string()),
            ..Default::default()
        },
        presence: status.map(|st| presence(id, st)),
    }))
}

fn list_update(guild: u64, ops: Vec<MemberListOp>) -> GatewayEvent {
    GatewayEvent::MemberListUpdate(Box::new(MemberListUpdate {
        id: "everyone".into(),
        guild_id: sf(guild),
        member_count: 3,
        online_count: 2,
        groups: vec![
            MemberListGroup {
                id: "online".into(),
                count: 2,
            },
            MemberListGroup {
                id: "offline".into(),
                count: 1,
            },
        ],
        ops,
    }))
}

fn rows(s: &Store, g: u64) -> Vec<String> {
    s.member_list(sf(g))
        .unwrap()
        .rows()
        .iter()
        .map(|r| match r {
            Some(ListRow::Group(g)) => format!("[{}]", g.id),
            Some(ListRow::Member(id)) => id.0.to_string(),
            None => "-".into(),
        })
        .collect()
}

fn synced_store() -> Store {
    let mut s = Store::new();
    s.apply(&ready(1, vec![guild(7, vec![])], vec![]));
    s.set_member_list_focus(Some(sf(7)));
    s.apply(&list_update(
        7,
        vec![MemberListOp::Sync {
            range: (0, 99),
            items: vec![
                group("online", 2),
                member_item(10, Some(Status::Online)),
                member_item(11, Some(Status::Dnd)),
                group("offline", 1),
                member_item(12, None),
            ],
        }],
    ));
    s
}

#[test]
fn member_list_sync_caches_members_and_presences() {
    let s = synced_store();
    assert_eq!(rows(&s, 7), vec!["[online]", "10", "11", "[offline]", "12"]);
    assert_eq!(s.status(sf(11)), Status::Dnd);
    assert_eq!(s.status(sf(12)), Status::Offline);
    assert_eq!(s.member_nick(sf(7), sf(11)), Some("Nick"));
    assert_eq!(s.user(sf(12)).unwrap().username, "user12");
    let list = s.member_list(sf(7)).unwrap();
    assert_eq!(list.total_rows(), 5);
    let sections = list.sections();
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].group.id, "online");
    assert_eq!(sections[0].group.count, 2);
    assert_eq!(sections[0].members, vec![sf(10), sf(11)]);
    assert_eq!(sections[1].members, vec![sf(12)]);
}

#[test]
fn member_list_insert_update_delete_invalidate() {
    let mut s = synced_store();
    // 12 comes online: Discord deletes the offline row and inserts it under online.
    s.apply(&list_update(
        7,
        vec![
            MemberListOp::Delete { index: 4 },
            MemberListOp::Insert {
                index: 1,
                item: member_item(12, Some(Status::Idle)),
            },
        ],
    ));
    assert_eq!(rows(&s, 7), vec!["[online]", "12", "10", "11", "[offline]"]);
    assert_eq!(s.status(sf(12)), Status::Idle);

    s.apply(&list_update(
        7,
        vec![MemberListOp::Update {
            index: 2,
            item: member_item(10, Some(Status::Dnd)),
        }],
    ));
    assert_eq!(s.status(sf(10)), Status::Dnd);
    assert_eq!(rows(&s, 7).len(), 5);

    // Out-of-range ops are harmless.
    s.apply(&list_update(
        7,
        vec![
            MemberListOp::Delete { index: 50 },
            MemberListOp::Update {
                index: 7,
                item: MemberListItem::Unknown,
            },
        ],
    ));
    assert_eq!(rows(&s, 7).len(), 5, "trailing unknown rows trimmed");

    s.apply(&list_update(
        7,
        vec![MemberListOp::Invalidate { range: (0, 99) }],
    ));
    assert!(rows(&s, 7).is_empty());
}

#[test]
fn member_list_resync_shorter_and_second_range() {
    let mut s = synced_store();
    s.apply(&list_update(
        7,
        vec![MemberListOp::Sync {
            range: (0, 99),
            items: vec![group("online", 1), member_item(10, Some(Status::Online))],
        }],
    ));
    assert_eq!(rows(&s, 7), vec!["[online]", "10"], "stale tail dropped");
    s.apply(&list_update(
        7,
        vec![MemberListOp::Sync {
            range: (100, 199),
            items: vec![member_item(20, None)],
        }],
    ));
    let r = rows(&s, 7);
    assert_eq!(r.len(), 101);
    assert_eq!(r[100], "20");
    assert_eq!(r[50], "-");
}

#[test]
fn member_list_is_capped() {
    let mut s = synced_store();
    let items: Vec<MemberListItem> = (0..100).map(|i| member_item(1000 + i, None)).collect();
    for start in [100u32, 200, 300, 400] {
        s.apply(&list_update(
            7,
            vec![MemberListOp::Sync {
                range: (start, start + 99),
                items: items.clone(),
            }],
        ));
    }
    assert_eq!(rows(&s, 7).len(), MEMBER_LIST_MAX_ROWS);
    s.apply(&list_update(
        7,
        vec![MemberListOp::Insert {
            index: 0,
            item: group("x", 1),
        }],
    ));
    assert_eq!(
        rows(&s, 7).len(),
        MEMBER_LIST_MAX_ROWS,
        "insert keeps the cap"
    );
}

#[test]
fn member_list_focus_ignores_and_drops_other_guilds() {
    let mut s = synced_store();
    // Updates for an unfocused guild are ignored.
    s.apply(&list_update(
        8,
        vec![MemberListOp::Sync {
            range: (0, 99),
            items: vec![member_item(30, Some(Status::Online))],
        }],
    ));
    assert!(s.member_list(sf(8)).is_none());
    assert!(s.presence(sf(30)).is_none());

    // A list with a different id needs a SYNC before it replaces the current one.
    let mut other = match list_update(7, vec![MemberListOp::Delete { index: 0 }]) {
        GatewayEvent::MemberListUpdate(u) => u,
        _ => unreachable!(),
    };
    other.id = "123".into();
    s.apply(&GatewayEvent::MemberListUpdate(other));
    assert_eq!(s.member_list(sf(7)).unwrap().id, "everyone");

    s.set_member_list_focus(Some(sf(8)));
    assert!(s.member_list(sf(7)).is_none(), "old list dropped");
    assert!(s.presence(sf(10)).is_none(), "its presences dropped");
    assert_eq!(
        s.member_nick(sf(7), sf(11)),
        None,
        "its member rows dropped"
    );
}

#[test]
fn member_list_drop_keeps_friends_and_message_members() {
    let mut s = synced_store();
    s.apply(&GatewayEvent::RelationshipAdd(Box::new(Relationship {
        id: sf(10),
        kind: RelationshipKind::Friend,
        user: None,
        nickname: None,
    })));
    let ch = chan(70, ChannelKind::Text, Some(7), None, 0);
    s.apply(&GatewayEvent::ChannelCreate(Box::new(ch)));
    let mut m = msg(1, 70, 11);
    m.guild_id = Some(sf(7));
    m.member = Some(nixcord_model::PartialMember {
        nick: Some("FromMsg".into()),
        ..Default::default()
    });
    s.insert_history(sf(70), vec![m]);
    s.set_member_list_focus(None);
    assert_eq!(s.status(sf(10)), Status::Online, "friend presence kept");
    assert_eq!(s.member_nick(sf(7), sf(11)), Some("FromMsg"));
}

#[test]
fn ready_bumps_epoch() {
    let mut s = Store::new();
    assert_eq!(s.epoch(), 0);
    s.apply(&ready(1, vec![], vec![]));
    s.apply(&ready(1, vec![], vec![]));
    assert_eq!(s.epoch(), 2);
}

// ---- permissions -----------------------------------------------------------------------

use nixcord_model::permissions::{ADMINISTRATOR, SEND_MESSAGES, VIEW_CHANNEL};

const ME: u64 = 1;
const G: u64 = 900;
const ROLE_A: u64 = 901;
const ROLE_B: u64 = 902;

fn prole(id: u64, perms: u64) -> nixcord_model::Role {
    nixcord_model::Role {
        id: sf(id),
        name: format!("r{id}"),
        permissions: perms,
        ..Default::default()
    }
}

fn ow(id: u64, kind: OverwriteKind, allow: u64, deny: u64) -> Overwrite {
    Overwrite {
        id: sf(id),
        kind,
        allow,
        deny,
    }
}

fn pchan(id: u64, parent: Option<u64>, ows: Vec<Overwrite>) -> Channel {
    Channel {
        permission_overwrites: ows,
        ..chan(id, ChannelKind::Text, Some(G), parent, id as i32)
    }
}

/// Guild G: @everyone can view + send; ROLE_A has nothing extra; ROLE_B is admin.
fn perm_store(my_roles: &[u64], owner: Option<u64>, channels: Vec<Channel>) -> Store {
    let mut g = guild(G, channels);
    g.owner_id = owner.map(sf);
    g.roles = vec![
        prole(G, VIEW_CHANNEL | SEND_MESSAGES),
        prole(ROLE_A, 0),
        prole(ROLE_B, ADMINISTRATOR),
    ];
    g.members = vec![nixcord_model::GuildMember {
        user: Some(user(ME)),
        roles: my_roles.iter().copied().map(sf).collect(),
        ..Default::default()
    }];
    let mut s = Store::new();
    s.apply(&ready(ME, vec![g], vec![dm(5, None, 2)]));
    s
}

fn deny_everyone_view() -> Overwrite {
    ow(G, OverwriteKind::Role, 0, VIEW_CHANNEL)
}

#[test]
fn permissions_everyone_deny_with_role_allow() {
    let ows = vec![
        deny_everyone_view(),
        ow(ROLE_A, OverwriteKind::Role, VIEW_CHANNEL, 0),
    ];
    let s = perm_store(
        &[ROLE_A],
        None,
        vec![pchan(10, None, ows.clone()), pchan(11, None, vec![])],
    );
    assert!(s.can_view(sf(10)), "role allow beats @everyone deny");
    assert!(s.can_send(sf(10)));
    assert!(s.can_view(sf(11)));
    let s = perm_store(&[], None, vec![pchan(10, None, ows)]);
    assert!(!s.can_view(sf(10)));
    assert!(!s.can_send(sf(10)), "no send without view");
}

#[test]
fn permissions_role_deny_and_member_overwrite() {
    // A role deny of SEND makes the channel read-only ...
    let ro = vec![ow(ROLE_A, OverwriteKind::Role, 0, SEND_MESSAGES)];
    let s = perm_store(&[ROLE_A], None, vec![pchan(10, None, ro.clone())]);
    assert!(s.can_view(sf(10)));
    assert!(!s.can_send(sf(10)));
    // ... and a member overwrite wins over role overwrites.
    let mut with_member = ro;
    with_member.push(ow(ME, OverwriteKind::Member, SEND_MESSAGES, 0));
    let s = perm_store(&[ROLE_A], None, vec![pchan(10, None, with_member)]);
    assert!(s.can_send(sf(10)), "member allow beats role deny");
    let ows = vec![
        deny_everyone_view(),
        ow(ME, OverwriteKind::Member, VIEW_CHANNEL | SEND_MESSAGES, 0),
    ];
    let s = perm_store(&[], None, vec![pchan(10, None, ows)]);
    assert!(s.can_view(sf(10)));
    assert!(s.can_send(sf(10)));
    let ows = vec![
        ow(ROLE_A, OverwriteKind::Role, VIEW_CHANNEL, 0),
        ow(ME, OverwriteKind::Member, 0, VIEW_CHANNEL),
    ];
    let s = perm_store(&[ROLE_A], None, vec![pchan(10, None, ows)]);
    assert!(!s.can_view(sf(10)), "member deny beats role allow");
    // A role overwrite for a role I don't have does nothing.
    let ows = vec![ow(ROLE_A, OverwriteKind::Role, 0, VIEW_CHANNEL)];
    let s = perm_store(&[], None, vec![pchan(10, None, ows)]);
    assert!(s.can_view(sf(10)));
}

#[test]
fn permissions_admin_and_owner_see_everything() {
    let ows = vec![
        deny_everyone_view(),
        ow(ME, OverwriteKind::Member, 0, VIEW_CHANNEL),
    ];
    let s = perm_store(&[ROLE_B], None, vec![pchan(10, None, ows.clone())]);
    assert!(s.can_view(sf(10)) && s.can_send(sf(10)), "administrator");
    let s = perm_store(&[], Some(ME), vec![pchan(10, None, ows)]);
    assert!(s.can_view(sf(10)) && s.can_send(sf(10)), "owner");
}

#[test]
fn permissions_fail_open_and_dms() {
    let ows = vec![deny_everyone_view()];
    let mut g = guild(G, vec![pchan(10, None, ows)]);
    g.roles = vec![prole(G, VIEW_CHANNEL)];
    let mut s = Store::new();
    s.apply(&ready(ME, vec![g], vec![dm(5, None, 2)]));
    assert_eq!(s.channel_permissions(sf(10)), None, "my member unknown");
    assert!(s.can_view(sf(10)) && s.can_send(sf(10)));
    assert!(s.can_view(sf(5)) && s.can_send(sf(5)), "DMs always");
    assert!(!s.can_view(sf(12345)), "unknown channel");

    // GUILD_MEMBER_UPDATE for me makes permissions known.
    s.apply(&GatewayEvent::GuildMemberUpdate {
        guild_id: sf(G),
        member: Box::new(nixcord_model::GuildMember {
            user: Some(user(ME)),
            ..Default::default()
        }),
    });
    assert!(!s.can_view(sf(10)));
}

#[test]
fn visible_channels_hide_denied_hidden_and_empty_categories() {
    let cat = |id: u64| Channel {
        kind: ChannelKind::Category,
        ..pchan(id, None, vec![])
    };
    let mut hidden_name = pchan(13, Some(100), vec![]);
    hidden_name.name = Some(HIDDEN_CHANNEL_NAME.into());
    let channels = vec![
        cat(100),
        pchan(11, Some(100), vec![]),
        pchan(12, Some(100), vec![deny_everyone_view()]),
        hidden_name,
        cat(200),
        pchan(21, Some(200), vec![deny_everyone_view()]),
        cat(300),
    ];
    let s = perm_store(&[], None, channels);
    let ids: Vec<u64> = s
        .visible_guild_channels(sf(G))
        .iter()
        .map(|c| c.id.0)
        .collect();
    assert_eq!(ids, vec![100, 11]);
}

#[test]
fn guild_emojis_update_replaces_list() {
    use nixcord_model::CustomEmoji;
    let emoji = |id: u64, name: &str| CustomEmoji {
        id: sf(id),
        name: name.into(),
        animated: false,
        available: true,
    };
    let mut s = Store::new();
    let mut g = guild(1, vec![]);
    g.emojis = vec![emoji(5, "ferris")];
    s.apply(&GatewayEvent::GuildCreate(Box::new(g)));
    assert_eq!(s.guild(sf(1)).unwrap().emojis.len(), 1);
    // GUILD_UPDATE without emojis keeps them.
    s.apply(&GatewayEvent::GuildUpdate(Box::new(guild(1, vec![]))));
    assert_eq!(s.guild(sf(1)).unwrap().emojis[0].name, "ferris");
    s.apply(&GatewayEvent::GuildEmojisUpdate {
        guild_id: sf(1),
        emojis: vec![emoji(6, "crab"), emoji(7, "party")],
    });
    let names: Vec<&str> = s
        .guild(sf(1))
        .unwrap()
        .emojis
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, ["crab", "party"]);
    // Unknown guild: ignored.
    s.apply(&GatewayEvent::GuildEmojisUpdate {
        guild_id: sf(9),
        emojis: vec![],
    });
}

// ---------------------------------------------------------------------------
// Read state, unread counts, mention counts & notifications
// ---------------------------------------------------------------------------

fn thread(id: u64, guild: u64, parent: u64, last: Option<u64>) -> Channel {
    Channel {
        id: sf(id),
        kind: ChannelKind::PublicThread,
        guild_id: Some(sf(guild)),
        name: Some(format!("t{id}")),
        parent_id: Some(sf(parent)),
        last_message_id: last.map(sf),
        ..Default::default()
    }
}

#[test]
fn ready_read_state_and_baseline() {
    use nixcord_model::ReadState;
    let mut c1 = chan(10, ChannelKind::Text, Some(100), None, 0);
    c1.last_message_id = Some(sf(500)); // has explicit read_state behind 500 -> unread + mentions
    let mut c2 = chan(11, ChannelKind::Text, Some(100), None, 1);
    c2.last_message_id = Some(sf(600)); // absent from read_state -> starts read (baseline = 600)
    let c3 = chan(12, ChannelKind::Text, Some(100), None, 2); // no messages yet -> not unread

    let mut unavailable_g = guild(200, vec![]);
    unavailable_g.unavailable = true;

    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(1),
        guilds: vec![guild(100, vec![c1, c2, c3]), unavailable_g],
        private_channels: vec![dm(20, Some(700), 2)],
        session_id: "s".into(),
        read_state: vec![
            ReadState {
                id: sf(10),
                last_message_id: Some(sf(400)),
                mention_count: 2,
            },
            // Read state for a channel in an unavailable guild should survive GUILD_CREATE.
            ReadState {
                id: sf(210),
                last_message_id: Some(sf(800)),
                mention_count: 3,
            },
            ReadState {
                id: sf(20),
                last_message_id: Some(sf(650)),
                mention_count: 1,
            },
        ],
        ..Default::default()
    })));

    assert_eq!(s.last_read_message_id(sf(10)), Some(sf(400)));
    assert!(s.channel_unread(sf(10)));
    assert_eq!(s.channel_mention_count(sf(10)), 2);

    assert_eq!(s.last_read_message_id(sf(11)), Some(sf(600)));
    assert!(!s.channel_unread(sf(11)));
    assert_eq!(s.channel_mention_count(sf(11)), 0);

    assert_eq!(s.last_read_message_id(sf(12)), None);
    assert!(!s.channel_unread(sf(12)));

    assert!(s.channel_unread(sf(20)));
    assert_eq!(s.dm_mention_count(), 1);
    assert_eq!(s.guild_unread_count(sf(100)), 1);
    assert!(s.guild_unread(sf(100)));
    assert_eq!(s.guild_mention_count(sf(100)), 2);
    // Unavailable guild 200 isn't counted yet until GUILD_CREATE arrives.
    assert_eq!(s.total_mention_count(), 3);

    // Late GUILD_CREATE for guild 200 preserves its READY read_state!
    let mut c210 = chan(210, ChannelKind::Text, Some(200), None, 0);
    c210.last_message_id = Some(sf(850));
    s.apply(&GatewayEvent::GuildCreate(Box::new(guild(200, vec![c210]))));
    assert!(s.channel_unread(sf(210)));
    assert_eq!(s.channel_mention_count(sf(210)), 3);
    assert_eq!(s.guild_mention_count(sf(200)), 3);
    assert_eq!(s.total_mention_count(), 6);
}

#[test]
fn live_messages_mentions_dms_ack_and_mark_read() {
    let mut g = guild(100, vec![chan(10, ChannelKind::Text, Some(100), None, 0)]);
    g.roles = vec![
        Role {
            id: sf(100),
            name: "@everyone".into(),
            permissions: perm::VIEW_CHANNEL | perm::SEND_MESSAGES,
            ..Default::default()
        },
        Role {
            id: sf(500),
            name: "devs".into(),
            position: 1,
            ..Default::default()
        },
    ];
    g.members = vec![GuildMember {
        user: Some(user(1)),
        roles: vec![sf(500)],
        ..Default::default()
    }];

    let mut s = Store::new();
    s.apply(&ready(1, vec![g], vec![dm(20, Some(50), 2)]));

    // Regular guild message -> unread, 0 mentions.
    let m1 = msg(100, 10, 2);
    s.apply(&GatewayEvent::MessageCreate(Box::new(m1)));
    assert!(s.channel_unread(sf(10)));
    assert_eq!(s.channel_mention_count(sf(10)), 0);
    assert_eq!(s.guild_unread_count(sf(100)), 1);

    // Direct mention of me -> +1.
    let mut m2 = msg(101, 10, 2);
    m2.mentions = vec![user(1)];
    s.apply(&GatewayEvent::MessageCreate(Box::new(m2.clone())));
    assert_eq!(s.channel_mention_count(sf(10)), 1);

    // Duplicate delivery of the same message must NOT double-count.
    s.apply(&GatewayEvent::MessageCreate(Box::new(m2)));
    assert_eq!(s.channel_mention_count(sf(10)), 1);

    // @everyone -> +1.
    let mut m3 = msg(102, 10, 2);
    m3.mention_everyone = true;
    s.apply(&GatewayEvent::MessageCreate(Box::new(m3)));
    assert_eq!(s.channel_mention_count(sf(10)), 2);

    // Role mention of a role I hold -> +1; unrelated role -> +0.
    let mut m4 = msg(103, 10, 2);
    m4.mention_roles = vec![sf(500)];
    s.apply(&GatewayEvent::MessageCreate(Box::new(m4)));
    assert_eq!(s.channel_mention_count(sf(10)), 3);

    let mut m5 = msg(104, 10, 2);
    m5.mention_roles = vec![sf(999)];
    s.apply(&GatewayEvent::MessageCreate(Box::new(m5)));
    assert_eq!(s.channel_mention_count(sf(10)), 3);

    // Incoming DM from another user -> increments DM mention count.
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(60, 20, 2))));
    assert!(s.channel_unread(sf(20)));
    assert_eq!(s.channel_mention_count(sf(20)), 1);
    assert_eq!(s.dm_mention_count(), 1);
    assert_eq!(s.total_mention_count(), 4);

    // Local mark_read clears unread and mentions for channel 10.
    s.mark_read(sf(10));
    assert!(!s.channel_unread(sf(10)));
    assert_eq!(s.channel_mention_count(sf(10)), 0);
    assert_eq!(s.last_read_message_id(sf(10)), Some(sf(104)));
    assert_eq!(s.total_mention_count(), 1);

    // Own message advances last_read and clears mentions on DM 20.
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(61, 20, 1))));
    assert!(!s.channel_unread(sf(20)));
    assert_eq!(s.dm_mention_count(), 0);
    assert_eq!(s.last_read_message_id(sf(20)), Some(sf(61)));

    // MESSAGE_ACK with current id marks read; MESSAGE_ACK with older id marks unread again.
    s.apply(&GatewayEvent::MessageCreate(Box::new(msg(110, 10, 2))));
    assert!(s.channel_unread(sf(10)));
    s.apply(&GatewayEvent::MessageAck {
        channel_id: sf(10),
        message_id: sf(110),
    });
    assert!(!s.channel_unread(sf(10)));
    s.apply(&GatewayEvent::MessageAck {
        channel_id: sf(10),
        message_id: sf(105),
    });
    assert!(s.channel_unread(sf(10)));
    assert_eq!(s.last_read_message_id(sf(10)), Some(sf(105)));

    // mark_read_to is monotonic (does not move backward).
    s.mark_read_to(sf(10), sf(110));
    assert_eq!(s.last_read_message_id(sf(10)), Some(sf(110)));
    s.mark_read_to(sf(10), sf(100));
    assert_eq!(s.last_read_message_id(sf(10)), Some(sf(110)));
}

#[test]
fn should_notify_rules() {
    let hidden = pchan(12, None, vec![deny_everyone_view()]);
    let visible = pchan(11, None, vec![]);
    let mut s = perm_store(&[], None, vec![visible, hidden]);
    s.apply(&GatewayEvent::ChannelCreate(Box::new(dm(20, None, 2))));

    // Non-mention in guild channel -> false.
    let plain = msg(1, 11, 2);
    assert!(!s.should_notify(&plain, None));

    // Direct mention -> true when not open, false when open.
    let mut ping = msg(2, 11, 2);
    ping.mentions = vec![user(ME)];
    assert!(s.should_notify(&ping, None));
    assert!(s.should_notify(&ping, Some(sf(20))));
    assert!(!s.should_notify(&ping, Some(sf(11))));

    // Own message -> false even if mentioning self or in DM.
    let mut own = msg(3, 20, ME);
    own.mentions = vec![user(ME)];
    assert!(!s.should_notify(&own, None));

    // DM from another user -> true unless open.
    let dm_msg = msg(4, 20, 2);
    assert!(s.should_notify(&dm_msg, None));
    assert!(!s.should_notify(&dm_msg, Some(sf(20))));

    // Hidden channel -> false even on @everyone.
    let mut hidden_ping = msg(5, 12, 2);
    hidden_ping.mention_everyone = true;
    assert!(!s.should_notify(&hidden_ping, None));

    // Do Not Disturb suppresses all notifications.
    s.set_my_status(Status::Dnd);
    assert!(!s.should_notify(&ping, None));
    assert!(!s.should_notify(&dm_msg, None));
}

// ---------------------------------------------------------------------------
// Threads lifecycle, ordering, permissions & cleanup
// ---------------------------------------------------------------------------

#[test]
fn threads_from_ready_events_and_sync() {
    use nixcord_model::ThreadMetadata;
    let mut g = guild(
        100,
        vec![
            chan(10, ChannelKind::Text, Some(100), None, 0),
            chan(11, ChannelKind::Text, Some(100), None, 1),
        ],
    );
    g.threads = vec![
        thread(50, 100, 10, Some(200)),
        thread(51, 100, 10, Some(300)),
        thread(52, 100, 11, Some(100)),
    ];

    let mut s = Store::new();
    s.apply(&ready(1, vec![g], vec![]));

    // Stored guild has threads emptied; guild_channels excludes threads.
    assert!(s.guild(sf(100)).unwrap().threads.is_empty());
    assert_eq!(chan_ids(&s.guild_channels(sf(100))), vec![10, 11]);
    // Ordered by last_message_id desc.
    assert_eq!(chan_ids(&s.channel_threads(sf(10))), vec![51, 50]);
    assert_eq!(chan_ids(&s.channel_threads(sf(11))), vec![52]);
    assert_eq!(chan_ids(&s.guild_threads(sf(100))), vec![51, 50, 52]);

    // GUILD_UPDATE without threads preserves existing threads.
    let mut g_upd = guild(100, vec![]);
    g_upd.name = "renamed".into();
    s.apply(&GatewayEvent::GuildUpdate(Box::new(g_upd)));
    assert_eq!(chan_ids(&s.guild_threads(sf(100))), vec![51, 50, 52]);

    // THREAD_CREATE adds a thread; message inside bumps it and rolls up mentions.
    s.apply(&GatewayEvent::ThreadCreate(Box::new(thread(
        53, 100, 10, None,
    ))));
    let mut tm = msg(400, 53, 2);
    tm.mentions = vec![user(1)];
    s.apply(&GatewayEvent::MessageCreate(Box::new(tm)));
    assert_eq!(chan_ids(&s.channel_threads(sf(10))), vec![53, 51, 50]);
    assert!(s.channel_unread(sf(53)));
    assert_eq!(s.guild_unread_count(sf(100)), 1);
    assert_eq!(s.guild_mention_count(sf(100)), 1);
    assert_eq!(s.total_mention_count(), 1);

    // THREAD_UPDATE archiving a thread without loaded history drops it cleanly.
    let mut arch50 = thread(50, 100, 10, Some(200));
    arch50.thread_metadata = Some(ThreadMetadata {
        archived: true,
        locked: false,
    });
    s.apply(&GatewayEvent::ThreadUpdate(Box::new(arch50)));
    assert!(s.channel(sf(50)).is_none());
    assert_eq!(chan_ids(&s.channel_threads(sf(10))), vec![53, 51]);

    // THREAD_UPDATE archiving a thread WITH loaded history hides it from lists but keeps channel.
    s.insert_history(sf(51), vec![msg(300, 51, 2)]);
    let mut arch51 = thread(51, 100, 10, Some(300));
    arch51.thread_metadata = Some(ThreadMetadata {
        archived: true,
        locked: false,
    });
    s.apply(&GatewayEvent::ThreadUpdate(Box::new(arch51)));
    assert!(s.channel(sf(51)).is_some());
    assert_eq!(chan_ids(&s.channel_threads(sf(10))), vec![53]);

    // Scoped THREAD_LIST_SYNC for parent 10 replaces parent 10's threads, leaving parent 11 untouched.
    s.apply(&GatewayEvent::ThreadListSync {
        guild_id: sf(100),
        channel_ids: vec![sf(10)],
        threads: vec![thread(54, 100, 10, Some(500))],
    });
    assert_eq!(chan_ids(&s.channel_threads(sf(10))), vec![54]);
    assert!(s.channel(sf(53)).is_none());
    assert!(s.channel(sf(51)).is_none());
    assert_eq!(chan_ids(&s.channel_threads(sf(11))), vec![52]);

    // THREAD_DELETE drops thread and its data.
    s.insert_history(sf(54), vec![msg(500, 54, 2)]);
    s.apply(&GatewayEvent::ThreadDelete {
        id: sf(54),
        guild_id: Some(sf(100)),
        parent_id: Some(sf(10)),
    });
    assert!(s.channel(sf(54)).is_none());
    assert!(s.messages(sf(54)).is_empty());

    // Deleting parent channel 11 drops its child thread 52.
    s.apply(&GatewayEvent::ChannelDelete(Box::new(chan(
        11,
        ChannelKind::Text,
        Some(100),
        None,
        1,
    ))));
    assert!(s.channel(sf(52)).is_none());
    assert!(s.guild_threads(sf(100)).is_empty());
}

#[test]
fn thread_permissions_inherit_parent_and_guild_delete_cleans_up() {
    let visible_parent = pchan(10, None, vec![]);
    let hidden_parent = pchan(11, None, vec![deny_everyone_view()]);
    let readonly_parent = pchan(
        12,
        None,
        vec![Overwrite {
            id: sf(G),
            kind: OverwriteKind::Role,
            allow: 0,
            deny: perm::SEND_MESSAGES,
        }],
    );
    let mut s = perm_store(
        &[],
        None,
        vec![visible_parent, hidden_parent, readonly_parent],
    );
    s.apply(&GatewayEvent::ThreadCreate(Box::new(thread(
        60,
        G,
        10,
        Some(1),
    ))));
    s.apply(&GatewayEvent::ThreadCreate(Box::new(thread(
        61,
        G,
        11,
        Some(2),
    ))));
    s.apply(&GatewayEvent::ThreadCreate(Box::new(thread(
        62,
        G,
        12,
        Some(3),
    ))));

    assert!(s.can_view(sf(60)) && s.can_send(sf(60)));
    assert!(!s.can_view(sf(61)) && !s.can_send(sf(61)));
    assert!(s.can_view(sf(62)) && !s.can_send(sf(62)));
    assert_eq!(chan_ids(&s.guild_threads(sf(G))), vec![62, 60]);
    assert!(s.channel_threads(sf(11)).is_empty());

    // GuildDelete drops all threads, their messages, and their read states.
    s.insert_history(sf(60), vec![msg(10, 60, 2)]);
    let mut ping = msg(11, 60, 2);
    ping.mentions = vec![user(ME)];
    s.apply(&GatewayEvent::MessageCreate(Box::new(ping)));
    assert_eq!(s.channel_mention_count(sf(60)), 1);

    s.apply(&GatewayEvent::GuildDelete {
        id: sf(G),
        unavailable: false,
    });
    assert!(s.channel(sf(60)).is_none());
    assert!(s.channel(sf(61)).is_none());
    assert!(s.messages(sf(60)).is_empty());
    assert_eq!(s.channel_mention_count(sf(60)), 0);
    assert!(s.guild_threads(sf(G)).is_empty());
}

// ---------------------------------------------------------------------------
// Profile popout query helpers
// ---------------------------------------------------------------------------

#[test]
fn profile_helpers_roles_relationships_and_snapshot() {
    let mut g = guild(100, vec![chan(10, ChannelKind::Text, Some(100), None, 0)]);
    g.roles = vec![
        Role {
            id: sf(100),
            name: "@everyone".into(),
            color: 0,
            position: 0,
            ..Default::default()
        },
        Role {
            id: sf(2),
            name: "helper".into(),
            color: 0x00FF00,
            position: 1,
            ..Default::default()
        },
        Role {
            id: sf(3),
            name: "mod-newer".into(),
            color: 0x0000FF,
            position: 5,
            ..Default::default()
        },
        Role {
            id: sf(1),
            name: "mod-older".into(),
            color: 0xFF0000,
            position: 5,
            ..Default::default()
        },
    ];
    g.members = vec![GuildMember {
        user: Some(user(42)),
        roles: vec![sf(100), sf(2), sf(3), sf(1)],
        nick: Some("Ace".into()),
        avatar: Some("guild_av".into()),
        ..Default::default()
    }];

    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(1),
        guilds: vec![g],
        relationships: vec![Relationship {
            id: sf(42),
            kind: RelationshipKind::Friend,
            user: Some(user(42)),
            nickname: None,
        }],
        presences: vec![Presence {
            user_id: sf(42),
            guild_id: None,
            status: Status::Idle,
            ..Default::default()
        }],
        ..Default::default()
    })));

    // member_roles excludes @everyone, orders position desc, tie-breaks id asc.
    let role_names: Vec<&str> = s
        .member_roles(sf(100), sf(42))
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(role_names, vec!["mod-older", "mod-newer", "helper"]);
    assert!(s.member_roles(sf(100), sf(999)).is_empty());

    assert_eq!(s.relationship_kind(sf(42)), RelationshipKind::Friend);
    assert_eq!(s.relationship_kind(sf(999)), RelationshipKind::None);

    // Guild-scoped profile.
    let p = s.user_profile(sf(42), Some(sf(100))).unwrap();
    assert_eq!(p.user.id, sf(42));
    assert_eq!(p.nick, Some("Ace"));
    assert_eq!(p.member_avatar, Some("guild_av"));
    assert_eq!(p.roles.len(), 3);
    assert_eq!(p.color, Some(0xFF0000));
    assert_eq!(p.status, Status::Idle);
    assert!(p.presence.is_some());
    assert_eq!(p.relationship, RelationshipKind::Friend);
    assert!(!p.is_me);

    // Non-guild profile (e.g. DM).
    let dm_p = s.user_profile(sf(42), None).unwrap();
    assert!(dm_p.nick.is_none());
    assert!(dm_p.member_avatar.is_none());
    assert!(dm_p.roles.is_empty());
    assert!(dm_p.color.is_none());
    assert_eq!(dm_p.status, Status::Idle);
    assert_eq!(dm_p.relationship, RelationshipKind::Friend);

    // Self profile & unknown user.
    assert!(s.user_profile(sf(1), None).unwrap().is_me);
    assert!(s.user_profile(sf(999), Some(sf(100))).is_none());
}

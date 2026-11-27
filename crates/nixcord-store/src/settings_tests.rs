//! Per-server notification settings: folding READY `user_guild_settings` and
//! USER_GUILD_SETTINGS_UPDATE, mute expiry and notification level resolution. Invented data.

use super::*;
use nixcord_model::Ready;

const G: u64 = 1;
const G2: u64 = 2;
const CATEGORY: u64 = 30;
/// A text channel inside `CATEGORY`.
const TEXT: u64 = 20;
/// A text channel without a category.
const LOOSE: u64 = 21;
/// A thread in `TEXT`.
const THREAD: u64 = 25;
const DM: u64 = 50;

const NOW: i64 = 1_767_323_045;

fn sf(n: u64) -> Snowflake {
    Snowflake(n)
}

fn channel(id: u64, kind: ChannelKind, parent: Option<u64>) -> Channel {
    Channel {
        id: sf(id),
        kind,
        parent_id: parent.map(sf),
        ..Default::default()
    }
}

fn over(channel: u64, muted: bool, level: NotificationLevel) -> ChannelOverride {
    ChannelOverride {
        channel_id: sf(channel),
        muted,
        mute_end: None,
        message_notifications: level,
    }
}

fn settings(guild: Option<u64>) -> UserGuildSettings {
    UserGuildSettings {
        guild_id: guild.map(sf),
        ..Default::default()
    }
}

fn store(user_guild_settings: Vec<UserGuildSettings>) -> Store {
    let guild = |id: u64, channels: Vec<Channel>, threads: Vec<Channel>| Guild {
        id: sf(id),
        name: format!("g{id}"),
        channels,
        threads,
        ..Default::default()
    };
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: User {
            id: sf(10),
            username: "me".into(),
            ..Default::default()
        },
        guilds: vec![
            guild(
                G,
                vec![
                    channel(CATEGORY, ChannelKind::Category, None),
                    channel(TEXT, ChannelKind::Text, Some(CATEGORY)),
                    channel(LOOSE, ChannelKind::Text, None),
                ],
                vec![channel(THREAD, ChannelKind::PublicThread, Some(TEXT))],
            ),
            guild(G2, vec![channel(40, ChannelKind::Text, None)], Vec::new()),
        ],
        private_channels: vec![channel(DM, ChannelKind::Dm, None)],
        session_id: "s".into(),
        user_guild_settings,
        ..Default::default()
    })));
    s
}

fn update(s: &mut Store, settings: UserGuildSettings) {
    s.apply(&GatewayEvent::UserGuildSettingsUpdate(Box::new(settings)));
}

#[test]
fn nothing_known_means_nothing_muted_or_suppressed() {
    let s = store(Vec::new());
    for guild in [Some(sf(G)), None, Some(sf(999))] {
        assert!(!s.guild_muted(guild, NOW));
        assert!(!s.suppress_everyone(guild));
        assert!(!s.suppress_roles(guild));
    }
    for ch in [TEXT, THREAD, DM, 999] {
        assert!(!s.channel_muted(sf(ch), NOW));
    }
    assert_eq!(s.notification_level(sf(TEXT)), NotificationLevel::Inherit);
    assert_eq!(s.notification_level(sf(DM)), NotificationLevel::AllMessages);
    assert_eq!(s.notification_level(sf(999)), NotificationLevel::Inherit);
}

#[test]
fn ready_settings_are_folded_per_scope() {
    let s = store(vec![
        UserGuildSettings {
            muted: true,
            suppress_everyone: true,
            message_notifications: NotificationLevel::OnlyMentions,
            ..settings(Some(G))
        },
        UserGuildSettings {
            suppress_roles: true,
            channel_overrides: vec![over(DM, true, NotificationLevel::Inherit)],
            ..settings(None)
        },
        // Says nothing: not stored.
        settings(Some(G2)),
    ]);
    assert!(s.guild_muted(Some(sf(G)), NOW));
    assert!(s.suppress_everyone(Some(sf(G))) && !s.suppress_roles(Some(sf(G))));
    assert!(!s.guild_muted(None, NOW));
    assert!(s.suppress_roles(None) && !s.suppress_everyone(None));
    assert!(s.channel_muted(sf(DM), NOW));
    // A muted server does not make its channels "channel muted".
    assert!(!s.channel_muted(sf(TEXT), NOW));
    assert!(!s.guild_muted(Some(sf(G2)), NOW));
    assert_eq!(s.guild_settings.len(), 2);
}

#[test]
fn update_replaces_and_a_new_ready_resets() {
    let mut s = store(vec![UserGuildSettings {
        muted: true,
        suppress_everyone: true,
        channel_overrides: vec![over(TEXT, true, NotificationLevel::Nothing)],
        ..settings(Some(G))
    }]);
    // The update is the whole object: what it does not say is off again.
    update(
        &mut s,
        UserGuildSettings {
            suppress_roles: true,
            ..settings(Some(G))
        },
    );
    assert!(!s.guild_muted(Some(sf(G)), NOW));
    assert!(!s.suppress_everyone(Some(sf(G))) && s.suppress_roles(Some(sf(G))));
    assert!(!s.channel_muted(sf(TEXT), NOW));
    assert_eq!(s.notification_level(sf(TEXT)), NotificationLevel::Inherit);
    // Other scopes are untouched, and a new one is added.
    update(
        &mut s,
        UserGuildSettings {
            muted: true,
            ..settings(None)
        },
    );
    assert!(s.guild_muted(None, NOW) && s.suppress_roles(Some(sf(G))));
    // Back to blank: the entry goes away.
    update(&mut s, settings(Some(G)));
    update(&mut s, settings(None));
    assert!(s.guild_settings.is_empty());

    update(
        &mut s,
        UserGuildSettings {
            muted: true,
            ..settings(Some(G))
        },
    );
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        session_id: "s2".into(),
        ..Default::default()
    })));
    assert!(!s.guild_muted(Some(sf(G)), NOW));
}

#[test]
fn mutes_expire_at_their_end_time() {
    let s = store(vec![UserGuildSettings {
        muted: true,
        mute_end: Some(NOW),
        channel_overrides: vec![
            ChannelOverride {
                mute_end: Some(NOW + 60),
                ..over(TEXT, true, NotificationLevel::Inherit)
            },
            // No end time: until unmuted.
            over(LOOSE, true, NotificationLevel::Inherit),
            // An end time on something not muted means nothing.
            ChannelOverride {
                mute_end: Some(NOW + 60),
                ..over(40, false, NotificationLevel::Nothing)
            },
        ],
        ..settings(Some(G))
    }]);
    assert!(s.guild_muted(Some(sf(G)), NOW - 1));
    assert!(
        !s.guild_muted(Some(sf(G)), NOW),
        "the end second is unmuted"
    );
    assert!(!s.guild_muted(Some(sf(G)), NOW + 1));

    assert!(s.channel_muted(sf(TEXT), NOW + 59));
    assert!(!s.channel_muted(sf(TEXT), NOW + 60));
    assert!(s.channel_muted(sf(LOOSE), i64::MAX));
    assert!(!s.channel_muted(sf(40), NOW));
}

#[test]
fn channel_mute_follows_parents() {
    let mut s = store(vec![UserGuildSettings {
        channel_overrides: vec![over(CATEGORY, true, NotificationLevel::Inherit)],
        ..settings(Some(G))
    }]);
    // A muted category mutes its channels and their threads, not loose channels.
    for ch in [CATEGORY, TEXT, THREAD] {
        assert!(s.channel_muted(sf(ch), NOW), "{ch}");
    }
    assert!(!s.channel_muted(sf(LOOSE), NOW));
    // A muted channel mutes its threads, not its category.
    update(
        &mut s,
        UserGuildSettings {
            channel_overrides: vec![
                over(TEXT, true, NotificationLevel::Inherit),
                // An expired mute further down does not hide the live one above.
                ChannelOverride {
                    mute_end: Some(NOW - 5),
                    ..over(THREAD, true, NotificationLevel::Inherit)
                },
            ],
            ..settings(Some(G))
        },
    );
    assert!(s.channel_muted(sf(TEXT), NOW) && s.channel_muted(sf(THREAD), NOW));
    assert!(!s.channel_muted(sf(CATEGORY), NOW));
}

#[test]
fn notification_level_resolves_override_then_parents_then_server() {
    use NotificationLevel::{AllMessages, Inherit, Nothing, OnlyMentions};
    let mut s = store(vec![
        UserGuildSettings {
            message_notifications: OnlyMentions,
            channel_overrides: vec![
                over(CATEGORY, false, Nothing),
                over(LOOSE, false, AllMessages),
                // Muted but no level of its own: the level still comes from above.
                over(TEXT, true, Inherit),
            ],
            ..settings(Some(G))
        },
        UserGuildSettings {
            message_notifications: Nothing,
            channel_overrides: vec![over(DM, false, OnlyMentions)],
            ..settings(None)
        },
    ]);
    assert_eq!(s.notification_level(sf(LOOSE)), AllMessages);
    assert_eq!(s.notification_level(sf(CATEGORY)), Nothing);
    assert_eq!(s.notification_level(sf(TEXT)), Nothing, "from the category");
    assert_eq!(s.notification_level(sf(THREAD)), Nothing, "two levels up");
    assert_eq!(s.notification_level(sf(DM)), OnlyMentions);
    // Another server's channels are not affected.
    assert_eq!(s.notification_level(sf(40)), Inherit);

    // Without overrides the server setting applies; private channels use theirs.
    update(
        &mut s,
        UserGuildSettings {
            message_notifications: OnlyMentions,
            ..settings(Some(G))
        },
    );
    update(
        &mut s,
        UserGuildSettings {
            message_notifications: Nothing,
            ..settings(None)
        },
    );
    for ch in [TEXT, LOOSE, THREAD] {
        assert_eq!(s.notification_level(sf(ch)), OnlyMentions, "{ch}");
    }
    assert_eq!(s.notification_level(sf(DM)), Nothing);
}

#[test]
fn leaving_a_guild_drops_its_settings_only() {
    let muted = |guild| UserGuildSettings {
        muted: true,
        ..settings(guild)
    };
    let mut s = store(vec![muted(Some(G)), muted(Some(G2)), muted(None)]);
    // An outage keeps them.
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(G),
        unavailable: true,
    });
    assert!(s.guild_muted(Some(sf(G)), NOW));
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(G),
        unavailable: false,
    });
    assert!(!s.guild_muted(Some(sf(G)), NOW));
    assert!(s.guild_muted(Some(sf(G2)), NOW) && s.guild_muted(None, NOW));
    assert_eq!(s.guild_settings.len(), 2);
}

#[test]
fn settings_do_not_change_unread_or_mention_counts() {
    let mut s = store(vec![UserGuildSettings {
        muted: true,
        suppress_everyone: true,
        message_notifications: NotificationLevel::Nothing,
        channel_overrides: vec![over(TEXT, true, NotificationLevel::Nothing)],
        ..settings(Some(G))
    }]);
    s.apply(&GatewayEvent::MessageCreate(Box::new(Message {
        id: sf(100),
        channel_id: sf(TEXT),
        guild_id: Some(sf(G)),
        author: User {
            id: sf(11),
            username: "other".into(),
            ..Default::default()
        },
        mention_everyone: true,
        ..Default::default()
    })));
    assert!(s.channel_unread(sf(TEXT)));
    assert_eq!(s.channel_mention_count(sf(TEXT)), 1);
}

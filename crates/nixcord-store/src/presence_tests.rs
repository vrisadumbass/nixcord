//! Presence scoping: overall (friend) vs per-guild presences, eviction, transitions.

use super::*;
use nixcord_model::{
    GuildMember, ListMember, MemberListItem, MemberListOp, MemberListUpdate, Ready,
    ReadySupplemental,
};

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

fn friend(id: u64) -> Relationship {
    Relationship {
        id: sf(id),
        kind: RelationshipKind::Friend,
        user: Some(user(id)),
        nickname: None,
    }
}

fn p(user: u64, guild: Option<u64>, status: Status) -> Presence {
    Presence {
        user_id: sf(user),
        guild_id: guild.map(sf),
        status,
        ..Default::default()
    }
}

fn update(user: u64, guild: Option<u64>, status: Status) -> GatewayEvent {
    GatewayEvent::PresenceUpdate {
        presence: Box::new(p(user, guild, status)),
        user: None,
    }
}

fn guild(id: u64) -> Guild {
    Guild {
        id: sf(id),
        name: format!("g{id}"),
        ..Default::default()
    }
}

/// Me (1); friends 20 and 21; DM with 30 (not a friend); guilds 100 and 101.
fn store(presences: Vec<Presence>) -> Store {
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(1),
        session_id: "s".into(),
        guilds: vec![guild(100), guild(101)],
        private_channels: vec![Channel {
            id: sf(500),
            kind: ChannelKind::Dm,
            recipients: vec![user(30)],
            ..Default::default()
        }],
        relationships: vec![friend(20), friend(21)],
        presences,
        ..Default::default()
    })));
    s
}

#[test]
fn per_guild_offline_does_not_hide_online_friend() {
    // The reported bug: a friend online overall, then a per-guild "offline" (they left
    // the guild, a member list row without presence, ...) wiped the only stored copy.
    let mut s = store(vec![
        p(20, Some(100), Status::Online),
        p(20, None, Status::Online),
    ]);
    assert_eq!(s.status(sf(20)), Status::Online);
    s.apply(&update(20, Some(100), Status::Offline));
    assert_eq!(s.status(sf(20)), Status::Online, "overall presence wins");
    // Per-guild idle is shadowed by the overall presence (which Discord also updates).
    s.apply(&update(20, Some(101), Status::Idle));
    assert_eq!(s.status(sf(20)), Status::Online);
    s.apply(&update(20, None, Status::Idle));
    assert_eq!(s.status(sf(20)), Status::Idle);
}

#[test]
fn overall_offline_then_online_again() {
    let mut s = store(vec![
        p(20, None, Status::Dnd),
        p(20, Some(100), Status::Dnd),
    ]);
    s.apply(&update(20, None, Status::Offline));
    assert_eq!(s.status(sf(20)), Status::Offline);
    assert!(
        s.presence(sf(20)).is_none(),
        "offline entries are not stored"
    );
    s.apply(&update(20, None, Status::Online));
    assert_eq!(s.status(sf(20)), Status::Online, "comes back");
    // A friend first seen through a guild-scoped update, with no guild_id-less one.
    s.apply(&update(21, Some(100), Status::Online));
    assert_eq!(s.status(sf(21)), Status::Online);
    // Invisible friends are reported as offline and stay offline.
    s.apply(&update(21, None, Status::Offline));
    assert_eq!(s.status(sf(21)), Status::Offline);
}

#[test]
fn dm_recipient_uses_latest_guild_copy() {
    // Non-friend DM recipients only get per-guild presences.
    let mut s = store(vec![
        p(30, Some(100), Status::Online),
        p(30, Some(101), Status::Online),
    ]);
    s.apply(&update(30, Some(100), Status::Offline));
    assert_eq!(
        s.status(sf(30)),
        Status::Online,
        "still online in guild 101"
    );
    s.apply(&update(30, Some(101), Status::Dnd));
    assert_eq!(s.status(sf(30)), Status::Dnd);
    s.apply(&update(30, Some(101), Status::Offline));
    assert_eq!(s.status(sf(30)), Status::Offline);
    assert!(s.presences.is_empty());
}

#[test]
fn leaving_a_guild_keeps_friend_presences() {
    let mut s = store(vec![
        p(20, None, Status::Online),
        p(30, Some(100), Status::Online),
        p(30, Some(101), Status::Idle),
        p(40, Some(100), Status::Online),
    ]);
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(100),
        unavailable: false,
    });
    assert_eq!(s.status(sf(20)), Status::Online, "friend kept");
    assert_eq!(s.status(sf(30)), Status::Idle, "other guild's copy kept");
    assert_eq!(
        s.status(sf(40)),
        Status::Offline,
        "copy of the left guild dropped"
    );
    // An outage keeps everything.
    s.apply(&GatewayEvent::GuildDelete {
        id: sf(101),
        unavailable: true,
    });
    assert_eq!(s.status(sf(30)), Status::Idle);
}

fn list(guild: u64, items: Vec<MemberListItem>) -> GatewayEvent {
    GatewayEvent::MemberListUpdate(Box::new(MemberListUpdate {
        id: "everyone".into(),
        guild_id: sf(guild),
        member_count: 2,
        online_count: 1,
        groups: vec![],
        ops: vec![MemberListOp::Sync {
            range: (0, 99),
            items,
        }],
    }))
}

fn row(id: u64, presence: Option<Presence>) -> MemberListItem {
    MemberListItem::Member(Box::new(ListMember {
        member: GuildMember {
            user: Some(user(id)),
            ..Default::default()
        },
        presence,
    }))
}

#[test]
fn member_list_presences_are_guild_scoped() {
    let mut s = store(vec![p(20, None, Status::Online)]);
    s.set_member_list_focus(Some(sf(100)));
    // List presences usually carry no guild_id; they must not become overall ones.
    s.apply(&list(
        100,
        vec![
            row(20, Some(p(20, None, Status::Offline))),
            row(21, None),
            row(40, Some(p(40, None, Status::Dnd))),
        ],
    ));
    assert_eq!(
        s.status(sf(20)),
        Status::Online,
        "friend not wiped by the list"
    );
    assert_eq!(s.status(sf(40)), Status::Dnd);
    assert_eq!(s.presence(sf(40)).unwrap().guild_id, Some(sf(100)));
    // Leaving the list drops only its copies of non-pinned users.
    s.set_member_list_focus(None);
    assert_eq!(s.status(sf(40)), Status::Offline);
    assert_eq!(s.status(sf(20)), Status::Online);
}

#[test]
fn unfriend_drops_overall_copy_and_supplemental_scopes() {
    let mut s = store(vec![]);
    s.apply(&GatewayEvent::ReadySupplemental(Box::new(
        ReadySupplemental {
            presences: vec![
                p(20, Some(100), Status::Online),
                p(20, None, Status::Idle),
                p(30, Some(101), Status::Dnd),
            ],
        },
    )));
    assert_eq!(s.status(sf(20)), Status::Idle);
    assert_eq!(s.status(sf(30)), Status::Dnd);
    s.apply(&GatewayEvent::RelationshipRemove { user_id: sf(20) });
    assert_eq!(
        s.status(sf(20)),
        Status::Offline,
        "no overall updates follow an unfriend"
    );
    s.apply(&update(20, Some(100), Status::Online));
    assert_eq!(
        s.status(sf(20)),
        Status::Online,
        "per-guild ones still count"
    );
}

#[test]
fn offline_with_activity_is_kept_and_own_status_is_separate() {
    let mut s = store(vec![]);
    let mut spotify = p(21, None, Status::Offline);
    spotify.activities = vec![nixcord_model::Activity {
        kind: nixcord_model::ActivityKind::Listening,
        name: "Spotify".into(),
        ..Default::default()
    }];
    s.apply(&GatewayEvent::PresenceUpdate {
        presence: Box::new(spotify),
        user: None,
    });
    assert_eq!(s.status(sf(21)), Status::Offline);
    assert_eq!(
        s.status_text(sf(21)).as_deref(),
        Some("Listening to Spotify")
    );
    assert_eq!(s.status(sf(1)), Status::Online, "own status from my_status");
}

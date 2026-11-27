//! Store folding of the M0 gateway events (roles, members, bans, bulk deletes, reaction
//! removal, pins, group DM recipients, relationship updates, USER_UPDATE). Invented data.

use super::*;
use nixcord_model::{permissions as perm, Ready};

const ME: u64 = 10;
const G: u64 = 1;
const TEXT: u64 = 20;
const GDM: u64 = 50;

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

fn role(id: u64, permissions: u64, color: u32, position: i32) -> Role {
    Role {
        id: sf(id),
        name: format!("r{id}"),
        color,
        position,
        permissions,
    }
}

fn member(id: u64, roles: &[u64]) -> GuildMember {
    GuildMember {
        user: Some(user(id)),
        roles: roles.iter().copied().map(sf).collect(),
        ..Default::default()
    }
}

fn msg(id: u64, channel: u64) -> Message {
    Message {
        id: sf(id),
        channel_id: sf(channel),
        author: user(9),
        timestamp: "2024-01-01T00:00:00Z".into(),
        ..Default::default()
    }
}

fn thumbs() -> Emoji {
    Emoji {
        id: None,
        name: Some("\u{1F44D}".into()),
        animated: false,
    }
}

fn custom() -> Emoji {
    Emoji {
        id: Some(sf(77)),
        name: Some("blob".into()),
        animated: false,
    }
}

/// Me in guild G with role 5 (the only role granting VIEW/SEND), a text channel, a group
/// DM with user 9, and user 9 as a member with colored role 6.
fn store() -> Store {
    let guild = Guild {
        id: sf(G),
        name: "g".into(),
        // @everyone (id == guild id) grants nothing.
        roles: vec![
            role(G, 0, 0, 0),
            role(5, perm::VIEW_CHANNEL | perm::SEND_MESSAGES, 0, 1),
            role(6, 0, 0x00FF00, 2),
        ],
        channels: vec![Channel {
            id: sf(TEXT),
            name: Some("text".into()),
            ..Default::default()
        }],
        members: vec![member(ME, &[5]), member(9, &[5, 6])],
        ..Default::default()
    };
    let gdm = Channel {
        id: sf(GDM),
        kind: ChannelKind::GroupDm,
        recipients: vec![user(9)],
        ..Default::default()
    };
    let mut s = Store::new();
    s.apply(&GatewayEvent::Ready(Box::new(Ready {
        user: user(ME),
        guilds: vec![guild],
        private_channels: vec![gdm],
        session_id: "s".into(),
        ..Default::default()
    })));
    s.insert_history(
        sf(TEXT),
        vec![msg(100, TEXT), msg(101, TEXT), msg(102, TEXT)],
    );
    s
}

fn react(s: &mut Store, message: u64, user_id: u64, emoji: Emoji) {
    s.apply(&GatewayEvent::ReactionAdd {
        channel_id: sf(TEXT),
        message_id: sf(message),
        user_id: sf(user_id),
        emoji,
    });
}

#[test]
fn role_update_recomputes_permissions() {
    let mut s = store();
    assert!(s.can_view(sf(TEXT)) && s.can_send(sf(TEXT)));
    // Role 5 loses SEND.
    s.apply(&GatewayEvent::GuildRoleUpsert {
        guild_id: sf(G),
        role: role(5, perm::VIEW_CHANNEL, 0, 1),
    });
    assert!(s.can_view(sf(TEXT)));
    assert!(!s.can_send(sf(TEXT)));
    // @everyone gains SEND back for everybody.
    s.apply(&GatewayEvent::GuildRoleUpsert {
        guild_id: sf(G),
        role: role(G, perm::SEND_MESSAGES, 0, 0),
    });
    assert!(s.can_send(sf(TEXT)));
}

#[test]
fn role_create_adds_and_colors() {
    let mut s = store();
    assert_eq!(s.member_color(sf(G), sf(9)), Some(0x00FF00));
    // A new, higher role the member does not have changes nothing...
    s.apply(&GatewayEvent::GuildRoleUpsert {
        guild_id: sf(G),
        role: role(7, perm::ADMINISTRATOR, 0xFF0000, 9),
    });
    assert_eq!(s.member_color(sf(G), sf(9)), Some(0x00FF00));
    assert_eq!(s.guild(sf(G)).unwrap().roles.len(), 4);
    // ...until they get it; then its color wins and an admin role means all permissions.
    s.apply(&GatewayEvent::GuildMemberUpdate {
        guild_id: sf(G),
        member: Box::new(member(ME, &[7])),
    });
    assert_eq!(s.channel_permissions(sf(TEXT)), Some(perm::ALL));
    s.apply(&GatewayEvent::GuildMemberUpdate {
        guild_id: sf(G),
        member: Box::new(member(9, &[5, 6, 7])),
    });
    assert_eq!(s.member_color(sf(G), sf(9)), Some(0xFF0000));
    // Recoloring the role shows at once.
    s.apply(&GatewayEvent::GuildRoleUpsert {
        guild_id: sf(G),
        role: role(7, perm::ADMINISTRATOR, 0x0000FF, 9),
    });
    assert_eq!(s.member_color(sf(G), sf(9)), Some(0x0000FF));
}

#[test]
fn role_delete_strips_permissions_color_and_member_roles() {
    let mut s = store();
    s.apply(&GatewayEvent::GuildRoleDelete {
        guild_id: sf(G),
        role_id: sf(5),
    });
    assert!(!s.can_view(sf(TEXT)));
    s.apply(&GatewayEvent::GuildRoleDelete {
        guild_id: sf(G),
        role_id: sf(6),
    });
    assert_eq!(s.member_color(sf(G), sf(9)), None);
    assert!(s.member_roles(sf(G), sf(9)).is_empty());
    // The id is gone from members too, so a role recreated with that id is not inherited.
    s.apply(&GatewayEvent::GuildRoleUpsert {
        guild_id: sf(G),
        role: role(5, perm::VIEW_CHANNEL, 0, 1),
    });
    assert!(!s.can_view(sf(TEXT)));
    // Unknown guild: ignored.
    s.apply(&GatewayEvent::GuildRoleDelete {
        guild_id: sf(999),
        role_id: sf(5),
    });
}

#[test]
fn member_add_caches_member_and_user() {
    let mut s = store();
    s.apply(&GatewayEvent::GuildMemberAdd {
        guild_id: sf(G),
        member: Box::new(GuildMember {
            nick: Some("newbie".into()),
            ..member(30, &[6])
        }),
    });
    assert_eq!(s.member_nick(sf(G), sf(30)), Some("newbie"));
    assert_eq!(s.member_color(sf(G), sf(30)), Some(0x00FF00));
    assert!(s.user(sf(30)).is_some());
}

#[test]
fn member_remove_and_ban_drop_member_and_guild_presence() {
    let mut s = store();
    for id in [30, 31] {
        s.apply(&GatewayEvent::GuildMemberAdd {
            guild_id: sf(G),
            member: Box::new(member(id, &[6])),
        });
        s.apply(&GatewayEvent::PresenceUpdate {
            presence: Box::new(Presence {
                user_id: sf(id),
                guild_id: Some(sf(G)),
                status: Status::Online,
                ..Default::default()
            }),
            user: None,
        });
        assert_eq!(s.status(sf(id)), Status::Online);
    }
    s.apply(&GatewayEvent::GuildMemberRemove {
        guild_id: sf(G),
        user_id: sf(30),
        banned: false,
    });
    s.apply(&GatewayEvent::GuildMemberRemove {
        guild_id: sf(G),
        user_id: sf(31),
        banned: true,
    });
    for id in [30, 31] {
        assert_eq!(s.member_color(sf(G), sf(id)), None);
        assert_eq!(s.status(sf(id)), Status::Offline);
    }
    // Unbanning stores nothing yet and must not panic.
    s.apply(&GatewayEvent::GuildBanRemove {
        guild_id: sf(G),
        user_id: sf(31),
    });
}

#[test]
fn member_remove_keeps_pinned_users_presence() {
    let mut s = store();
    // User 9 is a group DM recipient, so their presence is pinned.
    s.apply(&GatewayEvent::PresenceUpdate {
        presence: Box::new(Presence {
            user_id: sf(9),
            guild_id: Some(sf(G)),
            status: Status::Idle,
            ..Default::default()
        }),
        user: None,
    });
    s.apply(&GatewayEvent::GuildMemberRemove {
        guild_id: sf(G),
        user_id: sf(9),
        banned: false,
    });
    assert_eq!(s.status(sf(9)), Status::Idle);
    assert!(s.member_roles(sf(G), sf(9)).is_empty());
}

#[test]
fn user_update_replaces_me_and_cache() {
    let mut s = store();
    let renamed = User {
        global_name: Some("New Name".into()),
        avatar: Some("hash".into()),
        ..user(ME)
    };
    s.apply(&GatewayEvent::UserUpdate(Box::new(renamed.clone())));
    assert_eq!(s.me(), Some(&renamed));
    assert_eq!(s.user(sf(ME)), Some(&renamed));
    // Another user's object only refreshes the cache, never `me`.
    s.apply(&GatewayEvent::UserUpdate(Box::new(user(9))));
    assert_eq!(s.me().unwrap().id, sf(ME));
}

#[test]
fn message_delete_bulk_removes_only_listed() {
    let mut s = store();
    s.apply(&GatewayEvent::MessageDeleteBulk {
        ids: vec![sf(100), sf(102), sf(999)],
        channel_id: sf(TEXT),
    });
    let left: Vec<u64> = s.messages(sf(TEXT)).iter().map(|m| m.id.0).collect();
    assert_eq!(left, vec![101]);
    // Unknown channel: ignored.
    s.apply(&GatewayEvent::MessageDeleteBulk {
        ids: vec![sf(101)],
        channel_id: sf(12345),
    });
    assert_eq!(s.messages(sf(TEXT)).len(), 1);
}

#[test]
fn reaction_remove_all_clears_one_message() {
    let mut s = store();
    react(&mut s, 100, 9, thumbs());
    react(&mut s, 100, ME, custom());
    react(&mut s, 101, 9, thumbs());
    s.apply(&GatewayEvent::ReactionRemoveAll {
        channel_id: sf(TEXT),
        message_id: sf(100),
    });
    assert!(s.messages(sf(TEXT))[0].reactions.is_empty());
    assert_eq!(s.messages(sf(TEXT))[1].reactions.len(), 1);
}

#[test]
fn reaction_remove_emoji_clears_one_emoji() {
    let mut s = store();
    react(&mut s, 100, 9, thumbs());
    react(&mut s, 100, ME, thumbs());
    react(&mut s, 100, ME, custom());
    s.apply(&GatewayEvent::ReactionRemoveEmoji {
        channel_id: sf(TEXT),
        message_id: sf(100),
        emoji: thumbs(),
    });
    let r = &s.messages(sf(TEXT))[0].reactions;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].emoji.id, Some(sf(77)));
    assert!(r[0].me);
    // Custom emoji match on id even when the name differs.
    s.apply(&GatewayEvent::ReactionRemoveEmoji {
        channel_id: sf(TEXT),
        message_id: sf(100),
        emoji: Emoji {
            name: Some("renamed".into()),
            ..custom()
        },
    });
    assert!(s.messages(sf(TEXT))[0].reactions.is_empty());
}

#[test]
fn channel_pins_update_sets_and_clears() {
    let mut s = store();
    let ts = "2024-02-03T04:05:06+00:00".to_string();
    s.apply(&GatewayEvent::ChannelPinsUpdate {
        channel_id: sf(TEXT),
        last_pin_timestamp: Some(ts.clone()),
    });
    assert_eq!(s.channel(sf(TEXT)).unwrap().last_pin_timestamp, Some(ts));
    s.apply(&GatewayEvent::ChannelPinsUpdate {
        channel_id: sf(TEXT),
        last_pin_timestamp: None,
    });
    assert_eq!(s.channel(sf(TEXT)).unwrap().last_pin_timestamp, None);
}

#[test]
fn channel_recipients_add_and_remove() {
    let mut s = store();
    s.apply(&GatewayEvent::ChannelRecipientAdd {
        channel_id: sf(GDM),
        user: Box::new(user(40)),
    });
    // Adding twice does not duplicate.
    s.apply(&GatewayEvent::ChannelRecipientAdd {
        channel_id: sf(GDM),
        user: Box::new(user(40)),
    });
    let ids = |s: &Store| -> Vec<u64> {
        s.channel(sf(GDM))
            .unwrap()
            .recipients
            .iter()
            .map(|u| u.id.0)
            .collect()
    };
    assert_eq!(ids(&s), vec![9, 40]);
    assert!(s.user(sf(40)).is_some());
    s.apply(&GatewayEvent::ChannelRecipientRemove {
        channel_id: sf(GDM),
        user_id: sf(9),
    });
    assert_eq!(ids(&s), vec![40]);
}

#[test]
fn relationship_update_merges_partial() {
    let mut s = store();
    s.apply(&GatewayEvent::RelationshipAdd(Box::new(Relationship {
        id: sf(9),
        kind: RelationshipKind::IncomingRequest,
        user: Some(user(9)),
        nickname: None,
    })));
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(9),
        kind: Some(RelationshipKind::Friend),
        nickname: Some(Some("pal".into())),
    });
    let r = s.relationship(sf(9)).unwrap();
    assert_eq!(r.kind, RelationshipKind::Friend);
    assert_eq!(r.nickname.as_deref(), Some("pal"));
    // Absent type keeps it; null nickname clears it.
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(9),
        kind: None,
        nickname: Some(None),
    });
    let r = s.relationship(sf(9)).unwrap();
    assert_eq!(r.kind, RelationshipKind::Friend);
    assert_eq!(r.nickname, None);
    // Absent nickname keeps it.
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(9),
        kind: None,
        nickname: Some(Some("pal".into())),
    });
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(9),
        kind: Some(RelationshipKind::Blocked),
        nickname: None,
    });
    let r = s.relationship(sf(9)).unwrap();
    assert_eq!(r.kind, RelationshipKind::Blocked);
    assert_eq!(r.nickname.as_deref(), Some("pal"));
}

#[test]
fn relationship_update_for_unknown_user() {
    let mut s = store();
    // Without a type there is nothing to store.
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(70),
        kind: None,
        nickname: Some(Some("x".into())),
    });
    assert!(s.relationship(sf(70)).is_none());
    s.apply(&GatewayEvent::RelationshipUpdate {
        user_id: sf(70),
        kind: Some(RelationshipKind::OutgoingRequest),
        nickname: None,
    });
    assert_eq!(
        s.relationship_kind(sf(70)),
        RelationshipKind::OutgoingRequest
    );
}

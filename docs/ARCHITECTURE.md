# nixcord architecture

Goal: a native Discord client in Rust that idles under ~80 MB RAM, starts instantly,
sends no telemetry, and is easy to hack on.

## Crates

```
crates/
  nixcord-model    plain serde types (Snowflake, User, Guild, Channel, Message, GatewayEvent)
  nixcord-gateway  websocket: identify, heartbeat, resume, reconnect -> GatewayEvent stream
  nixcord-http     REST: history, send/edit/delete, typing, current user; rate limiting
  nixcord-store    pure in-memory state; folds GatewayEvents, caps messages per channel
  nixcord-capture  opt-in NIXCORD_CAPTURE: redacted gateway/REST payload shapes for fixtures
  nixcord-emoji    pure: unicode emoji table (emojibase), segmentation, search, Twemoji names
  nixcord-video    inline video (feature `video`): MP4/MOV demux, OpenH264, audio
  nixcord-app      egui/eframe UI, binary `nixcord`
```

Dependency direction: `{model, capture} <- {gateway, http, store} <- app`. Gateway, http
and store never depend on each other. `nixcord-capture` is a leaf (serde_json and tracing
only) so gateway and http can both call it; store does not use it.

## Runtime model

```
 UI thread (eframe)                          tokio runtime (background thread)
 ------------------                          ---------------------------------
 Store  <-- std/tokio mpsc: AppEvent  <---   gateway::run   (GatewayEvent)
        <--                           <---   http results   (History, Sent, Error)
 UI     ---> tokio mpsc: Command      --->   worker: LoadHistory, Send, Edit, Delete, Typing
```

- The UI owns the `Store` and mutates it only on the UI thread, draining the event
  channel at the start of each frame (`try_recv` loop). Backend tasks call
  `egui::Context::request_repaint()` after sending so the UI wakes up; when idle, the UI
  does not repaint (low CPU).
- The backend never touches UI state; the UI never awaits.

## Memory budget

- Messages: `MAX_MESSAGES_PER_CHANNEL = 250`, oldest evicted.
- Only rendered fields are deserialized (unknown JSON ignored).
- Presences: kept only while online or with an activity, per user and scope: the overall
  presence (friends / implicit relationships, no `guild_id`) wins and replaces per-guild
  copies; without one, per-guild copies are kept (latest shown). A per-guild "offline"
  drops only that guild's copy; an overall "offline" drops the user. Member list: only
  the focused guild, at most `MEMBER_LIST_MAX_ROWS = 300` rows (three 100-row ranges);
  switching guilds drops the old list and the per-guild presence copies only it
  referenced (friends and DM recipients are kept). Rows hold user ids only.
- Diagnostics: `NIXCORD_LOG=nixcord_gateway=debug,nixcord_store=debug` logs presence
  counts per READY / READY_SUPPLEMENTAL and each PRESENCE_UPDATE's status transition
  (ids and counts only); `NIXCORD_LOG_FILE=path` appends the log to a file (release
  builds have no console).
- Avatars/images: not in v0.1. Later: lazy-load, LRU texture cache with a byte cap.

## Gateway details

- `wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream`, with a streaming
  zlib inflater (`nixcord-gateway/src/inflate.rs`) flushed on the `00 00 ff ff` suffix,
  reset on every reconnect, capped at 4 MiB compressed / 16 MiB decompressed, and shrunk
  back to <= 64 KiB after large frames (e.g. `READY`). Plain text frames are still accepted.
- Hello (op 10) -> heartbeat every `heartbeat_interval` with jitter on the first beat;
  missed ACK (op 11) -> zombie connection -> reconnect + resume.
- Identify (op 2) with a user token and plain `properties` (`os`, `browser: "nixcord"`,
  `device: "nixcord"`). We do not send capabilities flags or telemetry.
- Resume (op 6) using `session_id`, `seq`, `resume_gateway_url`. Op 7 -> resume; op 9
  (invalid session) -> wait 1-5 s, re-identify unless `d == true`.
- Close codes 4004 -> `AuthFailed`, stop. 4010-4014 -> fatal, stop. Others -> backoff retry.
- App -> gateway commands (`run_with_commands`): op 37 Guild Subscriptions Bulk (member
  list ranges, typing, activities for the open guild channel) and op 3 Presence Update
  (own status + optional type-4 `"Custom Status"` activity, persisted in prefs and restored
  on `READY`). Coalesced and throttled (op 37 at most every 750 ms, op 3 every 4 s;
  Discord allows 5 presence updates per 20 s), sent only after READY/RESUMED.
- Decoded user-account events: PRESENCE_UPDATE, GUILD_MEMBER_LIST_UPDATE
  (SYNC/INSERT/UPDATE/DELETE/INVALIDATE), READY_SUPPLEMENTAL, RELATIONSHIP_ADD/REMOVE,
  SESSIONS_REPLACE (own status), GUILD_MEMBER_UPDATE, PRESENCES_REPLACE, GUILD_CREATE
  `presences`, MESSAGE_ACK, THREAD_CREATE/UPDATE/DELETE, THREAD_LIST_SYNC,
  CHANNEL_RECIPIENT_ADD/REMOVE (group DMs) and RELATIONSHIP_UPDATE (a partial
  relationship: absent keys keep the stored value, `nickname: null` clears it). Payload
  shapes and sources (official docs, Userdoccers) are documented in
  `nixcord-gateway/src/protocol.rs`.
- Also decoded: GUILD_ROLE_CREATE/UPDATE (one `GuildRoleUpsert` event) and
  GUILD_ROLE_DELETE, GUILD_MEMBER_ADD, GUILD_MEMBER_REMOVE and GUILD_BAN_ADD (one
  `GuildMemberRemove { banned }` event: the member and that guild's presence copy are
  dropped, pinned users keep theirs), GUILD_BAN_REMOVE (no-op until the bans list),
  USER_UPDATE (refreshes `me`), MESSAGE_DELETE_BULK, MESSAGE_REACTION_REMOVE_ALL /
  _REMOVE_EMOJI, and CHANNEL_PINS_UPDATE (`Channel::last_pin_timestamp`).
  GUILD_MEMBER_ADD is cached only when something can show the member: the focused
  member-list guild, or a user the store already holds (cached user, relationship, DM
  recipient, me).
- Notification settings: `READY.user_guild_settings` (versioned `{entries}` object or a
  bare array) and USER_GUILD_SETTINGS_UPDATE fold into per-scope settings (`None` scope:
  private channels). The store exposes `guild_muted`, `channel_muted` (own override, then
  parent channel and category), `suppress_everyone`, `suppress_roles` and
  `notification_level`; mute end times are Unix seconds and the current time is passed
  in. The update event replaces the whole entry. Unread and mention counts do not consult
  these yet (roadmap M2).
- Resilience: unknown events decode to `Ok(None)` and unknown fields are ignored; both
  are tested for every handled event in `nixcord-gateway/src/decode_fuzz.rs`, together
  with deterministic fuzzing of `decode_dispatch`, `dispatch_presences` and
  `Frame::parse` (mutated, random and truncated payloads must never panic).
- Permissions: the store computes VIEW_CHANNEL / SEND_MESSAGES with Discord's documented
  algorithm from roles, overwrites and our own member (READY `guilds[].members` /
  `merged_members`, GUILD_MEMBER_UPDATE). Thread channels inherit permissions from their
  parent channel; unknown member data fails open. Permissions and role colors are derived
  from `guild.roles` on every call, so role events only edit that list; a deleted role is
  also stripped from cached members.
- Read state & notifications: the store tracks per-channel `last_read_message_id` and
  `mention_count` from `READY.read_state`, live `MESSAGE_CREATE`, `MESSAGE_ACK`, and local
  `mark_read`, rolling unread and mention counts up per guild and across DMs.

## REST rate limits

`nixcord-http/src/ratelimit.rs`, one limiter shared by every `Http` clone, following
Discord's rate limit docs:

- Route key: method + path, with top-level resource ids (`channels`, `guilds`,
  `webhooks`) kept and other ids and reaction emoji folded. Requests on one route key
  run one at a time, in arrival order (a fair `tokio::sync::Mutex` per key).
- Buckets learned from `X-RateLimit-Bucket` / `-Remaining` / `-Reset-After`, tracked per
  (bucket, top-level resource). Before a route's first response it is limited under its
  own key.
- Global limit: 50 requests per second (one-second window); a global 429 (body
  `"global": true` or `X-RateLimit-Global`) holds every route until `retry_after`.
- 429s: a wait of at most 5 s is retried once, after the limiter has waited it out; a
  longer or repeated one is returned as `HttpError::RateLimited`.
- Fail fast: when a bucket or a global hold needs a wait longer than 5 s, later requests
  on it return `HttpError::RateLimited` at once instead of sleeping in the route queue.
  This also applies after a success that reported `remaining: 0` with a far reset.
- Errors: a Discord JSON error body (`{code, message, errors}`) becomes
  `HttpError::Api(ApiError)`; 401/404/429 and permission 403s (no code, 50001, 50013)
  keep their own variants. `HttpError::user_message()` gives the short sentence the app
  shows in toasts; it never contains raw bodies, URLs or transport detail.
- Maps are capped at 256 entries, expired ones pruned.

## Payload capture

`NIXCORD_CAPTURE=<dir>` (crate `nixcord-capture`) appends every incoming gateway dispatch
and every REST response body, redacted, to `<dir>/gateway.jsonl` and `<dir>/rest.jsonl`,
so real payload shapes can become test fixtures:

- Strings become `"<str>"` / `"<empty>"`, except a closed allowlist of enum values under
  specific keys: statuses under `status` and the `client_status` platform keys, member
  list `op`s, and member list group ids `online` / `offline`.
- Ids (15-20 digit strings, integers >= 10^15, id-like map keys) become salted hashes,
  consistent within one run; the salt is random per process. Permission bitfields are
  placeholders, never hashed.
- Epoch-looking integers and floats become a fixed time. Numbers under a closed list of
  keys that describe a real server or file (`color`, `position`, `member_count`, `count`,
  `width`, ...; see `ZEROED_NUMBER_KEYS`) become 0. Other numbers, booleans and nulls are
  kept (types, flags, enum levels).
- Object keys are kept only when they look like field names (`[a-z_][a-z0-9_]{0,47}`);
  others become `<key:N>`. A map keyed by a lowercase word of user data would still keep
  it, so read a capture before committing any of it as a fixture.
- REST paths keep only words from a closed route allowlist (`ROUTE_WORDS`); ids are
  hashed, everything else is `<seg>`, including whatever follows `invites` or `reactions`
  and webhook/interaction tokens. Queries and fragments are dropped.
- Never written: outgoing payloads (identify carries the token), request bodies, headers.
- Capped at 20 records per event type / route shape and 64 MiB in total.

## Themes

The app ships gradient themes: each theme has a two-stop background gradient, a panel
tint, an accent, and text colors. Painted as an egui `Mesh` behind transparent panels.

## Team / ownership (v0.1)

| Area                  | Owner           | Done when                                         |
|-----------------------|-----------------|---------------------------------------------------|
| model + contracts     | lead            | compiles, tests pass                              |
| gateway               | gateway agent   | unit tests for payload decoding + state machine   |
| http                  | http agent      | request building + error mapping tests            |
| store                 | store agent     | thorough unit tests (ordering, cap, dedupe)       |
| app / UI / themes     | ui agent        | `cargo run` shows login, sidebar, chat, composer  |
| integration, CI, docs | lead            | `cargo clippy -D warnings` + tests green in CI    |

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## Emoji

- `nixcord-emoji` (no UI deps): a ~120 KB string table generated by
  `crates/nixcord-emoji/tools/gen_data.py` from emojibase-data (shortcodes in Discord's
  JoyPixels style, names, keywords, Discord categories, skin-tone masks), indexed lazily
  (~60 KB). `emoji_runs` splits text into grapheme clusters and marks only clusters found
  in the table as emoji (text-default ones like `©` need U+FE0F).
- Color images are Twemoji 72x72 PNGs from jsDelivr (`jdecked/twemoji`, pinned tag),
  decoded to the drawn pixel size through `Media` (same LRU + disk cache as other images).
  Demo mode reads them from the disk cache only unless `NIXCORD_DEMO_NET=1`.
- `ui::emoji_text` draws single-line text with emoji (names, channels, statuses, reply
  previews); markdown reserves the same fixed squares inline, so layouts never shift when
  a texture arrives. Until then the font glyph is drawn.
- `ui::emoji_picker`: virtualized grid (only visible cells request 48 px textures),
  Frequently Used and skin tone persisted in prefs; composer autocomplete for `:name`.

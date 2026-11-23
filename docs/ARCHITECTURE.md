# nixcord architecture

Goal: a native Discord client in Rust that idles under ~80 MB RAM, starts instantly,
sends no telemetry, and is easy to hack on.

## Crates

```
crates/
  nixcord-model    plain serde types (Snowflake, User, Guild, Channel, Message, GatewayEvent)
  nixcord-gateway  websocket: identify, heartbeat, resume, reconnect -> GatewayEvent stream
  nixcord-http     REST: history, send/edit/delete, typing, current user
  nixcord-store    pure in-memory state; folds GatewayEvents, caps messages per channel
  nixcord-emoji    pure: unicode emoji table (emojibase), segmentation, search, Twemoji names
  nixcord-video    inline video (feature `video`): MP4/MOV demux, OpenH264, audio
  nixcord-app      egui/eframe UI, binary `nixcord`
```

Dependency direction: `model <- {gateway, http, store} <- app`. Gateway, http and store
never depend on each other.

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
  (own status). Coalesced and throttled (op 37 at most every 750 ms, op 3 every 4 s;
  Discord allows 5 presence updates per 20 s), sent only after READY/RESUMED.
- Decoded user-account events: PRESENCE_UPDATE, GUILD_MEMBER_LIST_UPDATE
  (SYNC/INSERT/UPDATE/DELETE/INVALIDATE), READY_SUPPLEMENTAL, RELATIONSHIP_ADD/REMOVE,
  SESSIONS_REPLACE (own status), GUILD_MEMBER_UPDATE, PRESENCES_REPLACE, GUILD_CREATE
  `presences`, MESSAGE_ACK, THREAD_CREATE/UPDATE/DELETE, and THREAD_LIST_SYNC. Payload
  shapes and sources are documented in `nixcord-gateway/src/protocol.rs`.
- Permissions: the store computes VIEW_CHANNEL / SEND_MESSAGES with Discord's documented
  algorithm from roles, overwrites and our own member (READY `guilds[].members` /
  `merged_members`, GUILD_MEMBER_UPDATE). Thread channels inherit permissions from their
  parent channel; unknown member data fails open.
- Read state & notifications: the store tracks per-channel `last_read_message_id` and
  `mention_count` from `READY.read_state`, live `MESSAGE_CREATE`, `MESSAGE_ACK`, and local
  `mark_read`, rolling unread and mention counts up per guild and across DMs.

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

## Roadmap after v0.1

Attachments and images, avatars, replies UI, markdown rendering, unread state, mentions,
zlib-stream compression, member list, plugins (a stable Rust trait API plus WASM).

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

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
- No member lists or presences in v0.1 (largest RAM consumers in the official client).
- Avatars/images: not in v0.1. Later: lazy-load, LRU texture cache with a byte cap.

## Gateway details

- `wss://gateway.discord.gg/?v=10&encoding=json`, no compression in v0.1.
- Hello (op 10) -> heartbeat every `heartbeat_interval` with jitter on the first beat;
  missed ACK (op 11) -> zombie connection -> reconnect + resume.
- Identify (op 2) with a user token and plain `properties` (`os`, `browser: "nixcord"`,
  `device: "nixcord"`). We do not send capabilities flags or telemetry.
- Resume (op 6) using `session_id`, `seq`, `resume_gateway_url`. Op 7 -> resume; op 9
  (invalid session) -> wait 1-5 s, re-identify unless `d == true`.
- Close codes 4004 -> `AuthFailed`, stop. 4010-4014 -> fatal, stop. Others -> backoff retry.

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

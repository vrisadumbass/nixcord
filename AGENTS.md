# AGENTS.md

A guide for AI coding agents (and humans) working on **nixcord**, a lightweight native
Discord client in Rust using egui/eframe. Read this first, then
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the deeper design.

## Ground rules

- **No personal data, ever.** Nothing from a real Discord account may enter the repo or
  its history. That covers usernames, server and channel names, message text (even
  paraphrased), and small details such as which emoji someone used. Demo data is invented,
  and screenshots come from `--demo` only. The history was rewritten once to remove such
  data, so don't reintroduce it.
- **Never log tokens.** `main.rs` caps `tungstenite`, `rustls`, `hyper`, `reqwest` and `h2`
  at `warn` no matter what `NIXCORD_LOG` says, because their debug output would include
  the token. Keep it that way. `Http` and the gateway never print the token, including
  in `Debug` output.
- **No self-bot features.** Nothing that automates a user account: no spam, scraping or
  auto-replies. See the README warning.
- **Lightweight is the product.** Keep the release exe around 15 MB and committed memory
  around 130–160 MB, most of which is the GPU driver. Don't cache what isn't rendered. Cap
  every cache by bytes. Repaint on events only, never continuously.
- **Licensing.** Every bundled asset or codec needs a license that allows redistribution.
  Record demo assets in `assets/demo/LICENSES.md` and codec/emoji credits in the README.
- **Commits.** Use the repo-local git identity. Add no AI attribution or co-author lines.
  Never commit build directories (`target*/`), screenshots of real accounts, or tokens.

## Commands

```sh
cargo run --release -p nixcord -- --demo   # fake data, no account needed
cargo run --release -p nixcord             # real login (token on the login screen or NIXCORD_TOKEN)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p nixcord --no-default-features --all-targets -- -D warnings   # without video
cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on Linux, Windows and macOS.
Linux needs `libasound2-dev` among others, because cpal is used for video audio.

When building while another nixcord is running, use a separate `CARGO_TARGET_DIR` (e.g.
`target-test`). Windows locks a running exe.

Environment variables:

| Var | Effect |
|---|---|
| `NIXCORD_TOKEN` | Log in without the login screen |
| `NIXCORD_DEMO=1` / `--demo` | Demo mode, with no Discord network traffic |
| `NIXCORD_DEMO_NET=1` | Demo may fetch Twemoji and public sample media |
| `NIXCORD_DEMO_VIDEO=<path>` | Adds a local video file to the demo |
| `NIXCORD_LOG=nixcord_gateway=debug,...` | Log filter; transport crates are always capped at `warn` |
| `NIXCORD_LOG_FILE=<path>` | Write the log to a file (release builds have no console) |
| `NIXCORD_RENDERER=glow` | Force OpenGL instead of Direct3D 12 on Windows |

## Layout

```
crates/
  nixcord-model     serde types: Snowflake, User, Guild, Channel, Message, Embed, components,
                    Presence, member-list ops, permissions, GatewayEvent, CDN URL helpers
  nixcord-gateway   websocket: zlib-stream decompression, identify, heartbeat/zombie detection,
                    resume, backoff, close codes; op 37 guild subscriptions, op 3 presence; GatewayCommand
  nixcord-http      REST: history, send/reply/edit/delete, reactions, typing, interactions,
                    acks, relationships, message search; 429 handling
  nixcord-store     pure state: messages (sliding window, cap 250/channel), read state &
                    mention counts, threads, presences (per-scope), member lists, permissions,
                    reactions, typing, emoji, profile helpers
  nixcord-markdown  Discord-flavored markdown -> AST (no egui), fuzz-tested
  nixcord-emoji     emoji table (emojibase), shortcodes, search, Twemoji filenames
  nixcord-media     fetch/decode/downscale, byte-capped LRU, disk cache, animations
  nixcord-video     MP4/MOV demux, OpenH264, AAC/MP3 via symphonia, cpal audio (feature "video")
  nixcord-app       egui app (binary `nixcord`)
    src/app.rs        NixApp: owns Store + Media, drains AppEvents, applies Actions
    src/backend.rs    tokio thread: gateway + REST worker (Command/AppEvent channels)
    src/demo.rs       fake Ready/messages (invented data only); demo_assets.rs loads assets/demo
    src/media.rs      textures, avatars, icons, Twemoji, image viewer state
    src/platform.rs   renderer choice (D3D12 on Windows with glow fallback), working-set trim
    src/ui/           sidebar, chat, composer, markdown, components, emoji_picker,
                      emoji_text, presence, profile, viewer, settings, login
assets/demo/        CC0 / public-domain / Apache-2.0 demo avatars, icons, photos, GIFs, emoji
docs/               ARCHITECTURE.md, screenshots/ (from --demo only)
```

Data flow: the UI thread owns the `Store`. The backend sends `AppEvent`s over a channel,
then calls `request_repaint()`. The UI sends `Command`s to the backend. UI code emits
`Action`s, which are applied after the frame.

## Done

- **Core:**
  - Token login, saved in the OS keychain.
  - Gateway reconnect/resume with `zlib-stream` transport compression.
  - Servers, channels, active threads and DMs.
  - History with scroll-back (a sliding window past the 250-message cap) and "Jump to present".
  - Send, edit (Up arrow), delete, reply.
  - Reactions, typing indicators, unread & mention badges (channels, threads, DMs, server rail,
    window title) and clickable in-app mention toasts.
- **Rendering:**
  - Markdown with syntax-highlighted code, spoilers, color-emoji mention pills and timestamps.
  - Embeds (rich and link) and attachments.
  - Message components: legacy action rows and V2 (container, section, gallery, file,
    separator, interactive buttons and string select menus).
  - Role colors, nicknames, APP badge.
- **Media:**
  - Avatars and server icons.
  - In-app image viewer (zoom, pan, save, copy link).
  - Animated GIF/WebP (Tenor/Giphy gifv mapped to animated WebP).
  - Animated emoji and avatars on hover.
  - Inline H.264 video with audio.
- **Emoji:**
  - Twemoji color emoji everywhere text appears, including mention pills and reaction pills
    (with missing-glyph fallbacks instead of box glyphs).
  - A picker with ranked search, categories, skin tones and server custom emoji.
  - `:shortcode:` autocomplete.
- **Presence & Profiles:**
  - Status dots, activities and custom status.
  - Own status picker (op 3).
  - Real member list via op 37 and GUILD_MEMBER_LIST_UPDATE, with role groups and Online/Offline.
  - Floating user profile popouts (avatar, status, custom status, activity, role pills, DM & copy actions).
  - Friends list from READY.
- **Permissions:** channels you can't view are hidden; read-only channels show the notice
  up front.
- **UI:**
  - Graphite (default) and Onyx themes, plus 12 gradient themes.
  - Settings modal.
  - UI zoom with presets; Ctrl +/−/0 stay in sync with the setting.
- **Memory:**
  - D3D12 with low-memory allocation hints; OpenGL fallback.
  - One working-set trim after startup.
  - Byte-capped caches throughout.

## Missing / known issues

Untested against real Discord. Most of these were built from community docs and
demo mode only:
- Op 37 subscriptions and member-list traffic, including unsubscribe behavior and range paging.
- The friends-showing-offline fix (per-scope presences). If it still happens, ask the user
  for a `NIXCORD_LOG_FILE` log with `nixcord_gateway=debug,nixcord_store=debug`, which
  holds ids and counts only.
- Permission overwrites on real servers.
- Video and GIFs from real Discord CDN attachments: the proxy poster, Range requests and
  `animated=true` on attachments.
- Save/copy in the image viewer, and drag-to-pan.
- `zlib-stream` gateway compression against live Discord.
- Live component interactions (`POST /interactions`), message acks (`POST /channels/{c}/messages/{m}/ack`),
  and `READY.read_state` / `MESSAGE_ACK` / `THREAD_*` events on real servers.

Not built yet:
- Voice and video calls, screen share.
- Native OS desktop notifications (in-app mention toasts, mention badges, unread counts and
  window title badges are built).
- Forum channels and thread creation/archiving UI (active text-channel threads are viewable
  and clickable).
- Entity select menus (user/role/mentionable/channel) and modals for bot interactions
  (buttons and string selects are interactive).
- Sending attachments and uploads, stickers, the GIF picker, message search UI (search REST
  routes exist in `nixcord-http`).
- Friends page UI actions (REST routes exist in `nixcord-http`), full profile modals
  (bio/banners/mutuals), server settings.
- Saving status changes to the account (currently session-only), custom status editing.
- Plugin API (planned: a stable Rust trait plus WASM).

UI polish backlog:
- The composer text box shows monochrome emoji (egui `TextEdit` can't draw textures).
- Demo still uses procedural images for "You", "Tamsin" and Coffee Break (assets not sourced).
- HEVC/VP9/AV1 video falls back to "Open in browser" (no decoder, by design).

## Working with agents on this repo

- Give each parallel agent its own files. Shared files (`app.rs`, `chat.rs`, `media.rs`,
  `demo.rs`, `model/lib.rs`) get small hunks only, re-read just before each edit.
- Agents verifying UI should screenshot only their own window (match on process id or exe
  path) and send input only to that window. Never click or type in other apps, and never
  close windows they didn't start. The user may be running a logged-in nixcord.
- The demo window shares eframe storage (theme, zoom) with the user's real app. Restore
  Graphite and 100% after testing.
- User reference screenshots are for layout and colors only. Never copy text, names or
  emoji from them.

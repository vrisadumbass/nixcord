# nixcord roadmap: to a full Discord client

Where we are: text chat, forums, threads, DMs, friends, search, media, bot components,
presence, the member list and voice calls (with DAVE E2EE) are built. Almost all of it
has only ever run in `--demo`. The road to parity is therefore two tracks: **prove what
exists against real Discord**, then **fill the feature gaps** in the order people hit
them in daily use.

Read [AGENTS.md](../AGENTS.md) for the ground rules. They apply to every item here.

## Rules for every milestone

1. **Verified live, or it isn't done.** A feature is done when it works in `--demo`
   *and* has been exercised on a real account, with the result ticked in this file.
2. **Demo first.** Every feature gets invented demo data, so screenshots and UI tests
   never need an account.
3. **Budgets hold.** Release exe ~15 MB, committed memory ~130-160 MB, repaint on events
   only. A feature that can't fit goes behind a cargo feature (as `video` and `voice` do).
4. **No personal data, no token logging, no self-bot features.** Captured fixtures hold
   ids and shapes only.
5. **Honest identification.** We identify as `nixcord` and send no telemetry. We do not
   imitate the official client's fingerprint. This is a known account risk and stays in
   the README warning.

## What "full port" means

Parity target: everything a regular user does in the desktop client day to day, plus
server moderation and administration.

Deliberately out of scope (link out to the browser instead):

| Not building | Why |
|---|---|
| Activities (embedded apps) | They are web apps in an iframe; we ship no webview |
| Nitro purchase, billing, Shop, Quests | Payment flows; open discord.com |
| Password login with captcha | hCaptcha needs a browser. Token login stays; QR login is planned (M2) |
| HEVC / VP9 / AV1 attachment video | No decoder, by design ("Open in browser") |
| Krisp noise suppression | Proprietary. An open alternative is planned (M5) |
| Developer portal, server discovery ads, telemetry | Not client features we want |

---

## M0: Live verification and protocol hardening (v0.5)

Nothing new ships until the existing surface is proven. This is the highest-risk
milestone, because every later one builds on these code paths.

- [ ] Work through the "Untested against real Discord" list in AGENTS.md, one row at a
      time: op 37 and member-list paging, per-scope presences, permission overwrites,
      CDN video/GIF (Range, proxy poster, `animated=true`), `zlib-stream`, interactions,
      acks, uploads, search and `around` jumps, friends/DM routes, forum and thread
      create/archive/lock, `READY.read_state`, GIF search, stickers, thread membership.
- [ ] Live voice: op 4 signaling, Voice Gateway v8, UDP/AEAD RTP, DAVE transitions,
      capture and playback on Windows, Linux and macOS.
*Built* below means implemented and unit-tested on invented data, but not yet exercised
on a real account. Per rule 1 such items stay unticked until the live check passes.

- [ ] Redacting capture tool: `NIXCORD_CAPTURE=<dir>` writes gateway/REST payloads with
      every string replaced by a placeholder and ids hashed, so real payload *shapes*
      become test fixtures without real data. *Built* (`685784d`, crate
      `nixcord-capture`; later tightened: route-word allowlist for paths, zeroed
      server-describing numbers, stricter keys); live check: capture a session and
      review the files.
- [ ] Missing gateway events: `GUILD_ROLE_CREATE/UPDATE/DELETE`,
      `GUILD_MEMBER_ADD/REMOVE`, `USER_UPDATE`, `MESSAGE_DELETE_BULK`,
      `MESSAGE_REACTION_REMOVE_ALL/_EMOJI`, `CHANNEL_PINS_UPDATE`,
      `CHANNEL_RECIPIENT_ADD/REMOVE`, `RELATIONSHIP_UPDATE`, `GUILD_BAN_ADD/REMOVE`.
      *Built* (`de15a5d`, decoded and folded into the store).
- [ ] `USER_GUILD_SETTINGS_UPDATE` and `READY.user_guild_settings`. *Built* (decoded and
      folded into the store with mute / notification-level accessors; unread counts do
      not use them until M2). Live check: whether real updates are full or partial
      objects.
- [ ] REST: per-route rate-limit buckets (`X-RateLimit-Bucket`), global limit, retry
      queue. *Built* (`e5cf138`, `nixcord-http/src/ratelimit.rs`; long waits now fail
      fast instead of sleeping in the queue).
- [ ] REST: typed error codes surfaced as toasts. *Built* (`HttpError::Api`,
      `user_message()`, used by every backend error toast).
- [ ] Resilience: unknown event and unknown field tolerance tests, gateway fuzzing of
      the dispatch decoder. *Built* (`606e80d`, `nixcord-gateway/src/decode_fuzz.rs`);
      tests only, so the live check is the captured fixtures decoding.
- [ ] Resilience: offline/reconnect banner. *Built* (the connection bar in `app.rs`
      already covers Connecting / Reconnecting / Offline); live check: pull the network.
- [x] Docs: fix stale README status, bump the workspace version, start a CHANGELOG.

**Exit:** a week of daily-driving text and voice on a real account without a crash, a
stuck reconnect or a wrong unread count.

## M1: Everyday messaging parity (v0.6)

The gaps a user notices in the first hour.

- [ ] Composer: `@user`, `@role`, `#channel` autocomplete (only `:emoji:` exists);
      color emoji in the text box (custom editor widget, replaces `TextEdit`);
      reply-ping toggle, `@silent`, spoiler and alt text on attachments, upload progress
      and size-limit errors, slowmode countdown.
- [ ] Message context menu: copy text / link / id, mark unread, pin, edit, delete,
      "who reacted" list, super-reaction display.
- [ ] Pins: pinned messages panel, pin/unpin, pin system messages.
- [ ] Forwarded messages (`message_snapshots`): render and send.
- [ ] Polls: render, vote, results, create.
- [ ] Voice messages: Ogg/Opus playback with waveform (decoder already in `nixcord-voice`).
- [ ] Stickers: Lottie and APNG rendering (evaluate a pure-Rust Lottie renderer against
      the size budget; keep the name card as fallback).
- [ ] Invite embeds, join-by-invite, message link previews, NSFW channel gate.
- [ ] All system message types (joins, boosts, pins, thread created, call started, etc.).
- [ ] Quick switcher (`Ctrl+K`), keyboard navigation (`Alt+Up/Down`, `Esc` to mark read),
      a shortcuts cheat sheet.

## M2: Notifications, settings sync and account (v0.7)

- [ ] Per-server and per-channel notification settings and mute (with durations), from
      `user_guild_settings`; suppress `@everyone` / role mentions.
- [ ] Native OS notifications (Windows toast, macOS, freedesktop) with click-to-jump,
      sounds, and a focus/do-not-disturb check.
- [ ] System tray with unread badge, close-to-tray, start minimized, single instance.
- [ ] Settings sync: decode the user-settings protobuf (folders, theme, status, favourite
      GIFs, frequently used emoji) and write back the parts we own.
- [ ] Inbox: mentions and unreads tabs.
- [ ] Account: QR-code login, multi-account switcher, edit own profile (avatar, display
      name, bio, pronouns), per-server profile and nickname, user notes.
- [ ] Privacy and safety settings, blocked/ignored users handling in chat.

## M3: Apps and bots (v0.8)

- [ ] Slash commands: command index per guild/DM, picker in the composer, all option
      types, option autocomplete, attachments as options.
- [ ] User and message context-menu commands.
- [ ] Modals (text inputs, selects, the newer label/file components).
- [ ] Ephemeral messages, deferred "thinking" state, interaction failure states.
- [ ] Remaining component types as Discord adds them; keep a compatibility table.
- [ ] App profiles, "Add App" link-out, user-installed apps in DMs.

## M4: Profiles, social and DMs (v0.9)

- [ ] Full profile modal: banner, bio, badges, mutual servers and friends, connections,
      roles, notes, activity.
- [ ] Group DMs: create, add/remove people, rename, icon, leave, ownership.
- [ ] Message requests and spam folder; friend nicknames; ignore.
- [ ] DM and group calls: ringing (`CALL_CREATE/UPDATE/DELETE`), accept/decline, call
      system messages.
- [ ] Rich presence display (assets, timestamps, party, buttons), Spotify-style cards,
      "active now" on the friends page.
- [ ] Optional local game detection for own rich presence (opt-in, off by default).

## M5: Voice completeness (v0.10)

- [ ] Push-to-talk and global hotkeys (mute, deafen, PTT) per platform.
- [ ] Per-user volume and local mute, input sensitivity UI, mic test, device hot-plug.
- [ ] Open noise suppression (RNNoise-class) and echo cancellation, behind a setting.
- [ ] Jitter buffer tuning, packet-loss concealment, FEC, reconnection across region
      moves, stats overlay.
- [ ] Stage channels: audience/speaker, raise hand, stage instances.
- [ ] Voice channel text chat, voice channel status, user limit, AFK channel.
- [ ] Soundboard: play and hear sounds.
- [ ] Server-side controls: move, server mute/deafen, disconnect (permission-gated).

## M6: Video and screen share (v0.11)

The largest single piece of work. Behind a `video-calls` cargo feature.

- [ ] Receive first: video RTP depacketization (H.264, then VP8), DAVE video frames,
      decode with the existing OpenH264 path, grid and focus layouts, pop-out.
- [ ] Stream watching: "Go Live" signaling (stream create/watch, separate stream
      server connection), stream previews.
- [ ] Send camera: capture (per-platform), H.264 encode, simulcast layers, bitrate control.
- [ ] Send screen: screen/window capture (Windows Graphics Capture, PipeWire portal,
      ScreenCaptureKit), application audio capture where the OS allows.
- [ ] Hardware decode/encode where available, with software fallback; measure the
      memory cost and document it.

## M7: Server administration and moderation (v0.12)

- [ ] Create/join/leave server, invites (create, list, revoke), server templates link-out.
- [ ] Channel and category management: create, edit, reorder, permissions editor.
- [ ] Roles: create, edit, reorder, permission matrix, assign to members.
- [ ] Member management: kick, ban, timeout, nickname, prune, member search (op 8).
- [ ] Server settings: overview, emoji/sticker/soundboard upload, webhooks and
      integrations, audit log viewer, AutoMod rules, bans list.
- [ ] Community features: onboarding, "Channels & Roles" browser, rules screening,
      announcement channel follow and publish, scheduled events, server boosts display.
- [ ] Thread and forum administration: tags, default reactions, slowmode, auto-archive.

## M8: Platform, accessibility and distribution (v0.13)

- [ ] Accessibility: AccessKit screen-reader support, full keyboard operation, reduced
      motion, contrast and font-scale checks.
- [ ] Localization: Fluent-based strings, language picker, RTL and CJK font fallback,
      IME correctness in the composer.
- [ ] Spellcheck (OS spellers), `discord://` and invite deep links.
- [ ] Packaging: signed Windows installer and portable exe, macOS notarized bundle,
      Linux AppImage/Flatpak, winget/Homebrew/AUR. Opt-in update check against GitHub
      releases only (documented in the README Network table).
- [ ] Performance gates in CI: exe size, cold start, idle memory, idle CPU at zero.
- [ ] Crash handling: local crash log file only, never uploaded.

## M9: Plugins and 1.0

- [ ] Plugin API v1: a stable Rust trait plus a WASM host with capability-scoped
      permissions (read messages in view, add UI panels, themes). No API that sends
      messages or acts without a user gesture, in keeping with the no-self-bot rule.
- [ ] Theme files (user-defined gradients and palettes) with hot reload.
- [ ] Parity audit: walk the official client menu by menu and file every difference.
- [ ] Security review of token storage, the WASM sandbox, media decoders and DAVE.
- [ ] 1.0: every box above ticked or explicitly moved to "out of scope".

---

## Ordering and dependencies

```
M0 ──► M1 ──► M2 ──► M3 ──► M4 ──► M7 ──► M8 ──► M9
 │                           │
 └──► M5 (voice) ────────────┴──► M6 (video)
```

- M0 gates everything.
- M5 can run in parallel with M1-M4: it touches `nixcord-voice` and `ui/voice.rs` only.
- M6 needs M5's transport work and M4's call signaling.
- M7 needs M0's role/member events and the rate-limit work.

## Working this roadmap with parallel agents

Each milestone splits by crate so agents don't collide (see "Working with agents" in
AGENTS.md). The usual shape is one "contracts" commit first (model types, `Command` /
`AppEvent` / `Action` variants, store method signatures), then one agent per area:

| Area | Files |
|---|---|
| Protocol | `nixcord-model`, `nixcord-gateway/src/protocol.rs`, `nixcord-http` |
| State | `nixcord-store` (pure, unit-tested) |
| UI feature | one new file under `nixcord-app/src/ui/` per feature |
| Demo | `demo.rs` data for the feature (small hunks) |
| Wiring | `app.rs`, `backend.rs` (lead only, small hunks) |

When a milestone ships: tick its boxes, move its lines into "Done" in AGENTS.md, and
update the README status.

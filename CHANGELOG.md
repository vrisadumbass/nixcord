# Changelog

Versions follow the milestones in [docs/ROADMAP.md](docs/ROADMAP.md). Everything here was
built and tested on invented data; "live" means checked on a real account.

## 0.5.0 (in development): M0, protocol hardening

Nothing in this release has been verified live yet.

### Added
- Gateway events: `GUILD_ROLE_CREATE/UPDATE/DELETE`, `GUILD_MEMBER_ADD/REMOVE`,
  `GUILD_BAN_ADD/REMOVE`, `USER_UPDATE`, `MESSAGE_DELETE_BULK`,
  `MESSAGE_REACTION_REMOVE_ALL/_EMOJI`, `CHANNEL_PINS_UPDATE`,
  `CHANNEL_RECIPIENT_ADD/REMOVE`, `RELATIONSHIP_UPDATE`.
- Notification settings: `READY.user_guild_settings` and `USER_GUILD_SETTINGS_UPDATE`,
  with store accessors for server/channel mute and notification level. Not yet used by
  unread or mention counts.
- REST rate limiting: per-route buckets, the global limit and a per-route queue.
- Typed Discord API errors (`HttpError::Api`) and plain-language error toasts.
- `NIXCORD_CAPTURE=<dir>`: redacted capture of payload shapes for test fixtures.
- Decoder resilience and fuzz tests for the gateway dispatch decoder.
- This changelog and the roadmap.
- M1 features, built and tested on invented data only: mention autocomplete for users, roles,
  `@everyone` / `@here` and channels; reply ping toggle; right-click message menu with Pin / Unpin,
  Mark Unread and Copy Message Link / ID; who-reacted list; polls with voting; forwarded messages;
  quick switcher (`Ctrl+K`), keyboard navigation and a shortcuts sheet (`Ctrl+/`).
- UI parity with the official client: floating user card, header Threads / notification / Pins
  buttons, server tags, nameplates, DM intro, device menus on mute and deafen.

### Fixed
- `Esc` now cancels a reply or edit, dismisses autocomplete and closes the header search.
- Mentions typed inside a reply now ping (replies send `allowed_mentions.parse`).
- A category is listed when any of its channels is visible; store, directory and unknown channel
  types are no longer listed.

### Changed
- A rate limit longer than 5 s now fails the request at once with a "slow down" message
  instead of leaving it waiting.
- A 403 that carries a specific Discord error code (for example "cannot send messages to
  this user") is reported as that error, not as a missing permission.
- Member joins are cached only for the focused server or for users already known.

## Before 0.5

Text chat, forums, threads, DMs, friends, search, media, bot components, presence, the
member list and voice calls with DAVE. See the git history and the "Done" list in
[AGENTS.md](AGENTS.md).

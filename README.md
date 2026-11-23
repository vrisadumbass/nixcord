# nixcord

A lightweight, native Discord client written in Rust. No Electron, no webview, no telemetry.

- **Native UI** with [egui](https://github.com/emilk/egui): starts instantly, sips RAM.
- **Zero telemetry.** The client talks to the Discord gateway and REST API, and nothing else.
- **Gradient themes**, a dozen built in, and easy to add your own.
- **Hackable.** A small crate-per-concern workspace with a pure, unit-tested state store.
  PRs welcome, forks welcome. It's open source, after all.

> [!WARNING]
> **Unofficial client. Use at your own risk.** Logging into Discord with a user token from a
> third-party client violates Discord's Terms of Service and can get your account
> suspended. nixcord is not affiliated with or endorsed by Discord Inc. It contains no
> self-bot, automation, spam, or scraping features, and contributions adding them will be
> rejected.

## Status

Early (v0.1). Working: login, guilds, channels, DMs, message history, sending, editing,
deleting, typing indicators, reconnect/resume, themes.
Not yet: images and avatars, markdown, reactions, voice, member list, notifications.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

## Build

Requires a recent stable Rust toolchain.

```sh
cargo run --release
```

Try the UI without an account (no network at all):

```sh
cargo run --release -- --demo
```

## Logging in

Paste your token on the login screen, or set `NIXCORD_TOKEN`. If you tick "remember", the
token goes into your OS keychain (Windows Credential Manager, macOS Keychain, Secret
Service), never into a plain file.

## Layout

```
crates/nixcord-model    serde data types
crates/nixcord-gateway  websocket gateway (identify, heartbeat, resume)
crates/nixcord-http     REST client
crates/nixcord-store    in-memory state, capped per channel
crates/nixcord-app      the egui app (binary: nixcord)
```

## Contributing

`cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`
must pass. Keep RAM in mind: don't cache what you don't render.

## License

Dual-licensed under MIT ([LICENSE](LICENSE)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.

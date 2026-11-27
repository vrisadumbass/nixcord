# Brief for parallel agents (milestone M1)

Shared rules for every agent working on a roadmap item in its own git worktree. Your task
prompt names your feature and the files you own; this file is the part that is the same
for everyone. Read [AGENTS.md](../AGENTS.md), [ARCHITECTURE.md](ARCHITECTURE.md) and
[ROADMAP.md](ROADMAP.md) first. The ground rules in AGENTS.md are hard constraints.

## Where you are

- You are in your own git worktree on your own branch. Other agents are working in other
  worktrees at the same time. The lead merges the branches afterwards.
- Nobody is watching. Don't ask questions and wait: make the call, write down what you
  chose and why, and carry on. Stop only if the task as written is impossible.

## Rules

1. **Invented data only.** Tests and demo data are made up. Nothing from a real Discord
   account, ever. Never log, print or write a token.
2. **No self-bot features.** Nothing that acts without a user gesture.
3. **Don't run the app.** Do not launch `nixcord` or any GUI, do not `cargo run`, do not
   take screenshots, and do not touch any running `nixcord` process: the owner may have a
   logged-in one open. The lead does the visual check after merging. Verify with unit
   tests and the headless egui render tests the UI files already use
   (`egui::Context::default()` + `ctx.run_ui(...)`; see the tests at the bottom of
   `crates/nixcord-app/src/ui/presence.rs` and `composer.rs`).
4. **Build with the shared target dir.** Set
   `CARGO_TARGET_DIR=C:\Users\nizam\Downloads\rustcord\target-test` for every cargo
   command. Other agents use it too, so cargo may wait on a lock: that is expected, just
   wait. Also set `__CARGO_DEFAULT_LIB_METADATA` to a value unique to you: worktrees have
   the same crate paths, so without it agents overwrite each other's artifacts and you
   see compile errors that are not yours. Never `cargo build` or `cargo run` the binary
   (they would overwrite a shared exe).
   Use `cargo clippy` and `cargo test` only.
5. **Stay in your files.** Your prompt lists what you own. In shared files (`app.rs`,
   `backend.rs`, `demo.rs`, `ui/mod.rs`, `ui/chat.rs`, `nixcord-model/src/lib.rs`,
   `nixcord-http/src/lib.rs`, `nixcord-store/src/lib.rs`,
   `nixcord-gateway/src/protocol.rs`) make small, additive hunks: add enum variants at the
   END of an enum, add functions at the END of an impl block, add match arms next to
   related ones. Don't reformat, reorder or refactor code you don't own. This keeps the
   merges clean. Put new logic in new files where you can.
6. **Don't edit docs.** Leave `AGENTS.md`, `README.md`, `CHANGELOG.md` and everything in
   `docs/` alone (several agents would collide). Put the doc lines you'd add in your
   final report instead.
7. **Dependencies.** Don't add crates. If you believe one is unavoidable, stop and say why
   and what it costs in binary size.
8. **Lightweight.** Don't cache what isn't rendered; cap anything that grows. Repaint on
   events only.
9. **Discord shapes.** Where you rely on a payload or route shape, write its source next
   to the code (official docs at docs.discord.com/developers, or Userdoccers at
   docs.discord.food for user-account behaviour). If you have web access, check; if you
   are working from memory, say so in the comment. Decode tolerantly: optional fields
   default, unknown fields are ignored, and a surprise in a decorative field must never
   fail a whole message or READY.
10. **Demo.** Add invented demo data so the feature is visible in `--demo`, and handle any
    new `Command` in `demo.rs` so the demo behaves like the real backend would.

## Before you finish

Run, and include the summary lines in your report:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p nixcord --no-default-features --all-targets -- -D warnings
cargo test --workspace
```

Then commit your work on your branch in one or a few commits, using the repo-local git
identity (it is already configured; don't change it). Commit messages are one plain
sentence in the imperative, with **no AI attribution and no co-author lines**. Do not
push. Do not merge or rebase onto other branches.

## Your report

Keep it short and factual:

- what you built, and the files you touched;
- every shared-file hunk (file and what you added), so the lead can resolve conflicts;
- what is tested and how; what you could not verify (anything visual, anything live);
- each Discord shape you relied on, its source, and how sure you are;
- the lines you would add to ROADMAP.md, AGENTS.md and CHANGELOG.md;
- your branch name and commit hashes.

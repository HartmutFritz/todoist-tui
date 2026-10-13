# Contributing

Thank you for helping improve `todoist-tui`.

## Development setup

Install Rust 1.88 or newer and Neovim, then run:

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --release
```

Use a disposable database while developing:

```sh
TODOIST_TUI_DB_PATH=/tmp/todoist-tui-development.db cargo run
```

The application works without a Todoist token. Never commit `API_TOKEN`, test
tokens, personal databases, or terminal recordings containing private tasks.

## Changes

- Keep input handling and local mutations independent of network access.
- Add regression tests for storage migrations and state-machine behavior.
- Preserve pending local operations during remote refreshes.
- Keep the default keymap and `config.example.toml` synchronized.
- Run formatting, tests, and strict Clippy before submitting a change.

Please describe user-visible behavior changes and any database migration in the
pull request.

## Architecture

- `main.rs` owns terminal setup and the adaptive event loop. Input must remain
  responsive while the application is idle or synchronizing.
- `app.rs` is the UI state machine. It operates only on local state and SQLite;
  it must never perform network requests.
- `store.rs` is the offline source of truth. User mutations and their outbox
  entries belong in the same SQLite transaction.
- `sync.rs` owns the single background synchronization worker, while
  `todoist.rs` contains the Todoist HTTP protocol implementation.
- `config.rs` resolves runtime paths and credentials; `keymap.rs` parses and
  validates user-configurable commands.
- `editor.rs` owns the embedded Neovim PTY and terminal emulation. `ui.rs`
  renders state and should not mutate application data apart from editor size.
  Configuration is validated before the terminal enters raw mode, so malformed
  configuration fails with a normal readable error.

Keep hot navigation paths allocation-light. Project movement should update the
title immediately and defer database-backed task loading through the existing
debounce. Prefer one SQLite connection per logical store operation and keep
remote work in the synchronization thread.

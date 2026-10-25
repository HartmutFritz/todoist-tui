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

CI also checks every target with Rust 1.88.0 and locked dependencies. Keep
dependency updates compatible with that minimum; run the equivalent check with
an installed Rust 1.88 toolchain when changing dependencies.

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
  renders state and updates presentation caches, scroll bounds, and editor size.
  Configuration is validated before the terminal enters raw mode, so malformed
  configuration fails with a normal readable error.

Keep hot navigation paths allocation-light. Project movement should update the
title immediately and defer database-backed task loading through the existing
debounce. Prefer one SQLite connection per logical store operation and keep
remote work in the synchronization thread.

The Todoist ordering migration is described in
[docs/ordering-migration.md](docs/ordering-migration.md), including API contracts,
compatibility requirements, and regression cases.
Offline recurrence sources and conservative preview limits are recorded in
[docs/recurrence-previews.md](docs/recurrence-previews.md).
The opt-in real-API ordering test and its isolated credential/database behavior
are documented in [docs/ordering-migration.md](docs/ordering-migration.md#opt-in-live-api-round-trip).

## Rendering measurements

Run the opt-in, disposable in-memory benchmark:

```sh
cargo test --locked render_benchmark -- --ignored --nocapture
```

It measures layout generation and cached viewport extraction for 10,000 tasks
with nesting depth 20, long titles, and label headings. It excludes database
loading, terminal writes, and event-loop latency. One local debug-profile run
measured about 198 ms per full layout before caching, a 152 ms cold cached layout,
and 13 µs per warm viewport redraw. These are observations, not timing assertions.

Invalidate `App::task_render_cache` whenever its task data changes. Width, view
modes, and date are part of its presentation key; focus and selection are applied
only to visible rows. Preserve the equivalent full-layout output in regressions.

Project pickers use `nucleo-matcher` in process, with one matcher allocation per
picker, prepared Unicode names, and ranked results cached by query. Their
candidate list is an immutable snapshot: reopen the picker to refresh project
names from the local cache.
Measure the 10,000-project fixture with:

```sh
cargo test --locked project_search_benchmark -- --ignored --nocapture
```

An optimized run (`cargo test --locked --release project_search_benchmark --
--ignored --nocapture`) measured 319 ms for 80 query updates over 10,000 decorated
project names (about 4 ms per update). Reusing cached results takes no rematching.
This measures matching/ranking only, not database loading or terminal rendering.

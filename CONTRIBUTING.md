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

### Nix packaging

With flakes enabled, `nix build` builds the locked package, including its Rust
tests. `nix flake check --print-build-logs` also runs installed-runtime checks in
a PTY: offline task creation/restart, packaged and user-provided Neovim, user
editor configuration, clipboard tool availability, and certificate overrides.
CI runs these checks natively on x86_64 Linux and ARM64 Linux.

`flake.lock` pins nixpkgs; `Cargo.lock` pins Cargo dependencies and their vendored
AWS-LC/SQLite sources. Update nixpkgs with `nix flake update nixpkgs`, then check
both platforms. `nix/package.nix` is reusable through `pkgs.callPackage`; the
flake also exports `overlays.default` exposing `pkgs.todoist-tui`.
`nix/source.nix` allowlists only public build inputs. Keep that list current when
adding compile-time assets, and never put credentials or local state in it.
Git-backed local flakes see new files only after they are included in Git;
avoid `path:.` in a checkout containing private files because Nix copies the
flake source before applying the package's source filter.

## Changes

- Keep input handling and local mutations independent of network access.
- Add regression tests for storage migrations and state-machine behavior.
- Preserve pending local operations during remote refreshes.
- Keep the default keymap and `config.example.toml` synchronized.
- Run formatting, tests, and strict Clippy before submitting a change.

Please describe user-visible behavior changes and any database migration in the
pull request.

Schema versions 11–12 add local completed-task visibility and alphabetical-sort
preferences keyed by view type and ID. The former global flags are discarded
because they cannot identify the project where they were enabled; unset views
default to hidden completed tasks and Todoist order. Task caches, outbox entries,
and global label-grouping preference are preserved. Project/filter ID remapping
and canonical Inbox reconciliation transfer both scoped preferences in the same
transaction, retaining explicit canonical destination choices.

Schema 13 caches active project collaborator membership from Sync's
`collaborator_states` alongside collaborator names. Upgrades request one full
refresh to backfill membership, retaining tasks, queued changes, and view
preferences. Task snapshots also retain assignment IDs for undo/redo and
recreation. The `+` picker reuses project fuzzy matching and revalidates cached
membership transactionally. Quick Edit supplies the original project context
for `+Name`, durably retains the parser text, and checkpoints assignment after
any project move. Replays must preserve later explicit assignment changes and
hydrate assignment IDs when loading checkpoints written by earlier releases.

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

The UI must not acquire synchronization-worker mutexes. Retry deadlines are
UI-owned snapshots published over a channel; preferred-filter updates travel in
the other direction and are coalesced by the worker. Keep storage I/O outside
coordination locks. `App::tick` processes at most eight sync notifications or
eight milliseconds of notification work before yielding to input/rendering, and
does no further refresh/storage work once quit is requested.

Task-details commands must target the dialog's current task ID, including search
results outside the active view. Share ID-targeted mutation helpers with the
task list; retain background selection for out-of-view targets. Hierarchy and
reordering use visible task order when available, otherwise the target project's
cached order and alphabetical preference.

Bulk rename freezes an ordered ID/title snapshot for its editor and draft
recovery. Saved unchanged lines are omitted; changed titles and outbox writes
commit in one transaction. `RenameTask` sends only literal content and protects
that field in parser/creation writebacks. Its session undo entry stores titles
only, so undo cannot overwrite metadata changed in the meantime. Keep bulk marks
separate from cursor selection and style them at viewport extraction time.

Filter refresh cadence is separate from freshness: successful outbox uploads and
manual sync invalidate favorite-filter membership immediately. Keep that request
pending through failed pushes/pulls and server backoff; periodic syncs alone use
the minute timer. Prioritize the selected filter, publish core/filter progress
before completed-history import finishes, and preserve UI selection and deferred
navigation when consuming it. History keeps its own persisted 15-minute success
or failure cooldown. Regression tests use gated mock responses to check visible
updates while the worker is still running, without sleeping for a polling minute.
Use the same outbox eligibility predicate for claiming and auxiliary yield
checks so quarantined operations do not starve unrelated filter refreshes.
Filter commits validate their query/favorite snapshot in the membership
transaction. Incomplete passes retain invalidation rather than advancing the
polling timer, and per-filter warnings survive interruption and restart.

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

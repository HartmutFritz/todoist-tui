# todoist-tui

Do you love Todoist, the terminal, and Neovim, but find it frustrating that
Todoist does not let you customize its keyboard shortcuts? Then `todoist-tui`
may be exactly what you are looking for. It is a keyboard-only TUI written in
Rust, with a rich set of useful shortcuts that let you move quickly between
projects and tasks. Every shortcut is fully customizable through a
configuration file, and tasks are added and edited in real, embedded Neovim
instances.

`todoist-tui` is a fast, offline-first Todoist client. It keeps a local SQLite
cache, applies changes immediately, and synchronizes with Todoist in the
background. The interface uses Vim-style navigation and embeds your normal
Neovim configuration for task and due-date editing.

## Features

- Fully usable offline; queued changes survive restarts.
- Non-blocking background synchronization every 60 seconds, with immediate
  sync after local changes and clear status reporting.
- Inbox, Today, Upcoming, projects, favorite filters, labels, due dates,
  priorities, and Quick Add.
- Ten project spaces plus a persistent, collapsible hidden-project group.
- Optional label grouping and alphabetical task ordering.
- Configurable keybindings and an in-app `?` help screen.
- Optional Quick Add shortcuts with per-key fallback projects.

## Requirements

- Rust 1.88 or newer
- Neovim (`nvim`) on `PATH`
- A Todoist API token for synchronization; the application also works without
  one in local-only mode

## Install and run

```sh
git clone https://github.com/HartmutFritz/todoist-tui.git
cd todoist-tui
cargo build --release
./target/release/todoist-tui
```

Create `API_TOKEN` beside `Cargo.toml` to enable synchronization:

```sh
printf '%s\n' 'your-todoist-api-token' > API_TOKEN
chmod 600 API_TOKEN
```

`TODOIST_API_TOKEN` can be used instead and takes precedence over the file.
Tokens are never printed or logged. Without a token, `todoist-tui` starts
normally and keeps changes locally.

## Interface

The left pane contains Inbox, the built-in Today and Upcoming views, favorite
filters, and projects. Today includes overdue tasks and tasks due today;
Upcoming includes tasks due today or later. Both views are calculated from the
local cache and remain available offline. The right pane contains tasks for the
selected view, and its selection is remembered independently for every view.

Projects are organized into spaces `0` through `9`:

- Inbox, Today, Upcoming, and favorite filters are always at the top of space
  `0`.
- Personal projects default to space `1`.
- Team/shared projects default to space `2`.
- `Ctrl+0` through `Ctrl+9` assigns or removes a local space override.
- `Ctrl+I` adds or removes the selected Inbox, Today, Upcoming, favorite filter,
  or project from the hidden group. `Shift+I` shows or hides that group.

Tasks are grouped by label by default. `Shift+M` toggles grouping and `Shift+N` toggles
alphabetical ordering. The modes are independent, so enabling both sorts tasks
alphabetically inside alphabetical label groups. Disabling both restores
Todoist order. Both choices are remembered across application restarts.
`Shift+C` shows or hides completed tasks; this choice is remembered too. Completed
tasks are grey and struck through. Press `d` on one to reopen it. Sync imports
completed tasks from the previous 89 days in addition to retaining the local
offline history.

`u` undoes the most recent task change from the current session. `Ctrl+R`
redoes an undone change. Adding, editing, changing due dates or labels,
completing/reopening, and moving tasks are included. A new task change clears
the redo history.

The bottom-left sync indicator is always visible:

- Green: synchronized and online.
- Orange: syncing or local changes are pending.
- Grey: Todoist is unreachable.
- Red: an actionable error, such as a missing/rejected token or API/storage
  failure. The accompanying text explains the problem.

Temporary action messages beside the indicator disappear after five seconds;
synchronization messages and errors remain visible until the state changes.

With an API token configured, synchronization starts at launch and repeats in
the background every 60 seconds. These scheduled refreshes are silent unless
they fail. Press `r` to synchronize immediately.

## Editing tasks

Press `q` or `a` to add a task. A compact Neovim window opens in Insert mode
and accepts a Todoist Quick Add expression:

```text
Buy milk tomorrow at 9 #Shopping @errands p1
```

From a filter, Inbox is the fallback project unless the expression names
another project. The full expression is queued unchanged when offline.

Optional special-add bindings open the same Quick Add editor but use a
configured project as the fallback. For example, pressing `w` can default the
new task to `#week`. A `#Project` typed in the Quick Add expression always
takes precedence over that fallback.

Press `o` to edit a task. The first line is its title and remaining lines are
its description. Press `t` to edit a due date using Todoist natural language,
such as `tod`, `tomorrow at 9`, or `every Friday`.

All editors start in Insert mode with the cursor at the useful editing
position. `Enter` saves in Insert or Normal mode; any other successful write
also accepts the value. Two consecutive `Esc` presses within 400 ms cancel the
editor; separated presses retain normal Neovim behavior. `:cq` also cancels.
Neovim loads your normal user configuration, plugins, and mappings.

Press `@` to search labels and toggle one on the selected task. Press `v` to
search projects and move the selected task. In either picker:

- Type a name prefix and press `Enter`, or
- press `Esc` once, choose with `j`/`k`, and press `Enter`.
- Press `Esc` again from selection mode to cancel.

## Default keybindings

Press `?` to see the active bindings and configuration path.

| Key | Action |
| --- | --- |
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `Shift+J` | Move down five projects or tasks |
| `Shift+K` | Move up five projects or tasks |
| `h` / `l` | Focus projects / tasks |
| `Enter` | Enter the task pane or show task details |
| `q` / `a` | Add a task with Quick Add |
| `o` | Edit title and description |
| `d` | Complete or reopen the selected task |
| `u` | Undo the last task change |
| `Ctrl+R` | Redo the last undone change |
| `t` | Edit the due date |
| `v` | Move the task to another project |
| `@` | Search and toggle a label |
| `Shift+M` | Toggle label grouping |
| `Shift+N` | Toggle alphabetical ordering |
| `Shift+C` | Show/hide completed tasks |
| `gg` / `Shift+G` | First / last row |
| `g` + at least two characters | Jump to a project or favorite filter |
| `Ctrl+0` … `Ctrl+9` | Assign/remove a project space override |
| `Shift+I` | Show/hide the hidden group |
| `Ctrl+I` | Add/remove the selected navigation item from the hidden group |
| `r` | Start background synchronization |
| `?` | Open help |
| `Shift+Q` | Quit |

`Esc` cancels prefix entry and controls picker mode. Help and task details
close with `Enter`, `Esc`, or `q`. Keys entered in Neovim belong to Neovim and
never trigger application bindings.

## Configuration

On Linux the default configuration path is
`~/.config/todoist-tui/config.toml`. Copy the complete example and restart
`todoist-tui`:

```sh
mkdir -p ~/.config/todoist-tui
cp config.example.toml ~/.config/todoist-tui/config.toml
```

Configuration is optional and may contain only the bindings you want to
change; omitted actions retain their defaults:

```toml
[keybindings]
down = ["j", "down", "ctrl+n"]
up = ["k", "up", "ctrl+p"]
add_task = ["a"]
quit = ["ctrl+c"]
```

Special-add bindings are empty by default. Map a single key to a local Todoist
project name in a separate table:

```toml
[special_add]
w = "#week"
n = "#notes"
```

Project lookup is case-insensitive and accepts names with or without `#`. The
project must already be present in the local cache. Special-add keys may not
conflict with regular main-screen bindings.

Project-specific jump prefixes can override name-prefix lookup. Map the text
typed after `g` to an exact project name:

```toml
[project_jumps]
ww = "#week"
```

With this example, `gww` jumps directly to `week`. Prefixes contain at least
two letters, are case-insensitive, and must not overlap another configured
project-jump prefix. A mapped project is excluded from normal name-prefix
matching, so its configured prefix is the only way to jump to it. The project
must be visible and its name must be unique.

Each array contains alternative bindings. Use `[]` to unbind an action. Key
names are case-insensitive and support `ctrl`, `alt`, `shift`, and `super`.
Named keys include `enter`, `esc`, arrows, `home`, `end`, `pageup`, `pagedown`,
`tab`, `backtab`, `insert`, `delete`, `backspace`, and `f1` through `f255`.
Uppercase letters may be written as `G` or `shift+g`.

A space inside one entry denotes a sequence. Only `first` supports a sequence,
and its first key must match `jump`:

```toml
[keybindings]
jump = ["g"]
first = ["g g"]
```

Unknown settings, malformed keys, and conflicting main-screen bindings produce
a startup error. See [config.example.toml](config.example.toml) for every
action.

## Data and environment

The SQLite database is stored under the platform data directory as
`todoist-tui/todoist-tui.db`.

| Variable | Purpose |
| --- | --- |
| `TODOIST_API_TOKEN` | Override `API_TOKEN` |
| `TODOIST_TUI_CONFIG_PATH` | Use a specific configuration file; it must exist |
| `TODOIST_TUI_DB_PATH` | Use a specific SQLite database path |

For example:

```sh
TODOIST_TUI_DB_PATH=/tmp/todoist-tui-test.db TODOIST_TUI_CONFIG_PATH="$PWD/config.example.toml" cargo run
```

`API_TOKEN`, local databases, and build artifacts are excluded from version
control. Back up the database if its offline-only changes are important.

## Development

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md). Licensed
under the [MIT License](LICENSE).

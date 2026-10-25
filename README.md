# todoist-tui

A keyboard-driven Todoist terminal client with Vim-style navigation,
customizable shortcuts, and embedded Neovim editing. Changes are saved locally
and synchronized in the background, so cached tasks remain usable offline.

<p align="center">
  <img src="docs/images/todoist-tui_1.png" alt="todoist-tui screenshot 1" width="49%">
  <img src="docs/images/todoist-tui_2.png" alt="todoist-tui screenshot 2" width="49%">
</p>
<p align="center">
  <img src="docs/images/todoist-tui_3.png" alt="todoist-tui screenshot 3" width="49%">
  <img src="docs/images/todoist-tui_4.png" alt="todoist-tui screenshot 4" width="49%">
</p>

## Install and run

Requirements:

- Rust 1.88 or newer to build the application.
- Neovim (`nvim`) on `PATH` for editing.
- For clipboard copying: `wl-copy` on Wayland, or `xclip`/`xsel` on X11.

Clipboard helpers run in the background with timeouts; an unavailable display
or hung helper does not block navigation or quitting.

```sh
git clone https://github.com/HartmutFritz/todoist-tui.git
cd todoist-tui
cargo build --locked --release
```

Save your [Todoist API token](https://app.todoist.com/app/settings/integrations)
in `API_TOKEN` in the cloned project directory, then start the application:

```sh
printf '%s\n' 'your-todoist-api-token' > API_TOKEN
chmod 600 API_TOKEN
./target/release/todoist-tui
```

Alternatively, set `TODOIST_API_TOKEN`. Without a token, the application starts
in local-only mode; add a token and restart to enable synchronization.

## Everyday use

The left pane contains Inbox, Today, Upcoming, favorite filters, and projects.
Today includes overdue tasks; Upcoming covers today and the next six days.
The right pane shows the selected view's tasks and remembers your selection.

Press `?` for all active shortcuts and the configuration path. Uppercase letters
below mean Shift+letter.

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | Move down / up |
| `h` / `l`, Left / Right | Focus navigation / tasks |
| Enter | Enter tasks, open task details, or confirm a picker |
| `gg` / `G` | First / last item |
| `g` + at least two characters | Jump to a uniquely matching project or favorite filter |
| `p` / `f` / `F` | Search projects / cached tasks / filters |
| `a` / `q` | Add a task at the top / bottom in Todoist order |
| `c` | Add a subtask |
| `e` | Edit a task, or a filter from its details |
| `d` | Complete / reopen a task |
| `D` | Delete a task and its subtasks |
| `t` / `T` | Edit / remove the due date |
| `@` / `v` | Toggle a label / move a task to another project |
| `>` / `<` | Indent / promote a task |
| `u` / Ctrl+r | Undo / redo a task change from this session |
| `L` / `A` / `C` | Toggle label grouping / alphabetical order / completed tasks |
| `P` / Ctrl+f | Add a project / personal filter |
| `R` | Rename the selected project |
| `r` | Synchronize |
| `J` / `K` | Move the focused project or task down / up among siblings |
| `y` | Copy the focused title |
| Ctrl+0 … Ctrl+9 | Set/remove a project's space override |
| Ctrl+i / `I` | Hide/unhide a navigation item / expand or collapse hidden items |
| `E` | Show the complete synchronization error |
| `S` | Inspect queued and rejected synchronization operations |
| `Q` | Quit |

Project pickers (`p` and `v`) use built-in, case-insensitive fuzzy matching: `wk`
finds `#💼 Work`, and `prj rpt` finds “Project reports.” Best matches appear first;
spaces separate search terms. Project search includes hidden projects. Other
pickers use name prefixes; task search includes cached completed tasks.
Type a search and press Enter, or press Esc once to select results with `j`/`k`.
A second Esc cancels.

With tasks focused, `v` moves to any other cached personal or team project,
including hidden projects. Type a fuzzy destination search and press Enter. Parent
moves include their subtasks; supported moves work offline and support undo/redo.
Todoist does not support moving individual tasks out of a team workspace; the
application explains that restriction before applying a known unsupported move.

Help and detail dialogs close with Enter, Esc, or `q`. Tasks that have a
description show a text indicator (`☰`) on the right; Enter opens the full
description.

Scroll task, filter, and error details with `j`/`k` (or your configured movement
keys) to read long titles, queries, and descriptions in small terminals.

Task titles wrap at word boundaries. A word too wide for a row appears as `…`;
open details to read the complete title.
Selection stays visible even in very large, heavily wrapped task lists.

Tasks are grouped by label by default. Sorting, grouping, and completed-task
visibility are remembered between runs. Subtasks stay with their parent.
Alphabetical sorting affects only the displayed order; `a` and `q` still record
top/bottom placement, which appears once sorting is disabled. In Upcoming,
date groups determine placement instead. Disable alphabetical sorting before
using `J`/`K`.

`J`/`K` keep task moves within the same project, section, and parent; project
moves stay among siblings in the same workspace and folder. Reordering and
top/bottom placement synchronize to other Todoist clients.

Personal projects default to space `1`, team/shared projects to `2`; built-in
views and favorite filters lead space `0`. Space overrides and hidden membership
are saved locally. Children follow their root project's space even when an
ancestor is hidden.

## Adding and editing

`a`, `q`, and `c` open Neovim with your usual configuration. The first line
accepts Todoist Quick Add syntax; following lines are the description:

```text
Buy milk tomorrow at 9 #Shopping @errands p1
Two bottles of oat milk.
```

The selected project is the fallback destination; from Today, Upcoming, or a
filter, the fallback is Inbox. An explicit `#Project` takes precedence.
Natural-language metadata is confirmed by Todoist during synchronization.

Use `e` to edit a task's title and description; metadata not mentioned in the
edited expression is preserved. Use `t` for a due expression such as `tod`,
`tomorrow at 9`, or `every Friday`. Empty due input removes the date.

Every editor starts in Insert mode:

- Enter inserts a newline by default.
- **Esc, then Enter saves and closes** through Normal mode. `:w` also saves.
- Shift+Enter in Insert mode always inserts a newline, even with save-on-Enter
  enabled. This requires a terminal that reports Shift+Enter distinctly.
- Two consecutive Esc presses within 400 ms, or `:cq`, cancel.

Validation errors or unsuccessful local saves retain your draft for correction
or retry. If Neovim cannot reopen, the recovery dialog offers Enter/`e` to retry
and Esc/`q` to discard it. Background refresh errors leave an active editor open.

Only favorite filters appear in navigation. Use `F` to find any cached filter,
Enter to open its details, and `e` to edit it. Frozen filters are read-only.
Filter editors require exactly three lines; use arrows to move between fields:

```text
Filter title
(today | overdue) & #Work
favorite = true
```

Use `favorite = false` for a non-favorite filter. Removing the current filter
from favorites switches to a surviving view and restores its task selection.
Unavailable views leave the task pane empty.

## Configuration

On Linux, configuration is optional and defaults to
`~/.config/todoist-tui/config.toml`. Copy [config.example.toml](config.example.toml)
there, or create a file containing only your changes. Restart after editing it.

```toml
[editor]
save_on_enter_in_insert = false # Set true to save with Insert-mode Enter too.
visual_line_navigation = true  # Add gj/gk fallbacks for otherwise-unmapped j/k.

[keybindings]
down = ["j", "down", "ctrl+n"]
up = ["k", "up", "ctrl+p"]
quit = ["ctrl+c"]

[special_add]
w = "#week" # Open Quick Add with this existing project as the fallback.

[project_jumps]
ww = "#week" # Type gww to jump to this unique visible project.
```

Each binding accepts alternatives; `[]` unbinds an action. Omitted settings keep
their defaults. See the example file for all actions and supported key names.
Normal-mode Enter always saves, regardless of `save_on_enter_in_insert`.

Bind symbols using their displayed character, such as `?`, `@`, or `+`;
`ctrl++` means Ctrl+plus. Shift+Tab and BackTab are equivalent. Embedded Neovim
supports standard xterm function keys F1–F63; higher numbers are ignored there.

Special-add keys must not conflict with regular bindings. Project lookups ignore
case and accept names with or without `#`. Custom jump prefixes must contain at
least two supported characters and cannot overlap; mapped projects are excluded
from ordinary name-prefix jumps.

## Synchronization and data

Synchronization starts at launch, after changes, and every 60 seconds. Local
changes survive restarts. The bottom-left indicator shows the current state:

| Indicator | Meaning |
| --- | --- |
| Green **Synced** | Online with no queued changes |
| Orange **Pending** / **Syncing** | Changes are waiting / synchronization is running |
| Grey **Offline** | Todoist is unreachable; local use remains available |
| Red **Error** | Read the accompanying explanation; `E` opens full details |

Correct a rejected change and retry with `r` from the task pane. Unrelated
changes can continue synchronizing. Automatic retries respect server-requested
delays. Failed filter/history refreshes retain cached results and report
staleness. Completed history initially includes the previous 89 days and
refreshes at most every 15 minutes.

For an old “Moving from a workspace is not implemented” rejection, use `u` if
that move is still in this session's undo history, or find the task at its cached
destination and use `v` to move it back to a project in its original team. Then
synchronize. This safely removes the rejected move's queue blocker; pressing `r`
alone cannot make Todoist support the original move. See
[Todoist's limitation](https://www.todoist.com/help/todoist/teams/move-a-team-project-to-your-personal-projects-Kw3GqRNlC).

Use `S` to inspect the local queue without synchronizing: `j`/`k` select an
operation, `h`/`l` change pages, and Enter opens its full error, dependency, and
progress details. Enter/Esc/`q` returns to the list; Esc/`q` closes it. From the
list, `r` refreshes the snapshot. These keys follow your configured bindings.

Recurring tasks remain active after completion. Common all-day rules preview the
next occurrence locally, including day/week intervals and weekday lists such as
`every mon, fri`, safe month/year intervals, and `every!`/`after` intervals.
Plain daily/weekly and explicit completion-relative rules follow completion day;
scheduled intervals retain their phase. Explicit ISO start/end bounds are
supported. With an absolute timestamp, `every! 3 hours` previews elapsed time.
Ambiguous month-end/leap-day dates, floating/local-clock times, and unsupported
rules wait for Todoist confirmation.

On Linux, the database defaults to
`~/.local/share/todoist-tui/todoist-tui.db` (respecting `XDG_DATA_HOME`). Back it up
to preserve offline-only changes. Use a separate database for each Todoist
account; an account mismatch stops synchronization.

Existing caches upgrade automatically. The next synchronization refreshes
ordering information while retaining queued local changes.

| Environment variable | Purpose |
| --- | --- |
| `TODOIST_API_TOKEN` | Token override; takes precedence over `API_TOKEN` |
| `TODOIST_TUI_CONFIG_PATH` | Custom configuration file; it must exist |
| `TODOIST_TUI_DB_PATH` | Custom database path |

Licensed under the [MIT License](LICENSE).

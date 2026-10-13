use chrono::Local;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        App, Focus, LabelPickerItem, Overlay, PickerMode, ProjectPickerItem, SyncStatus,
        filtered_label_indices, filtered_project_indices,
    },
    keymap::{Action, KeyBindings},
    model::{NavRow, Task},
};

const BG: Color = Color::Rgb(20, 20, 22);
const PANEL: Color = Color::Rgb(27, 27, 30);
const RED: Color = Color::Rgb(164, 49, 61);
const RED_BRIGHT: Color = Color::Rgb(211, 84, 96);
const SELECT: Color = Color::Rgb(91, 34, 42);
const TEXT: Color = Color::Rgb(226, 222, 220);
const MUTED: Color = Color::Rgb(139, 134, 132);
const GOLD: Color = Color::Rgb(209, 166, 73);
const SYNC_GREEN: Color = Color::Rgb(76, 187, 123);
const SYNC_ORANGE: Color = Color::Rgb(224, 151, 62);
const SYNC_RED: Color = Color::Rgb(222, 76, 86);
const TASK_LOOKAHEAD: usize = 4;
const LIST_TOP_PADDING: u16 = 1;
const TASK_RIGHT_PADDING: u16 = 2;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(frame.area());
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(31), Constraint::Percentage(69)])
        .split(vertical[0]);
    draw_navigation(frame, app, columns[0]);
    draw_tasks(frame, app, columns[1]);
    draw_status(frame, app, vertical[1]);
    draw_overlay(frame, app);
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_set(symbols::border::ROUNDED)
        .border_style(Style::default().fg(if focused { RED_BRIGHT } else { RED }))
        .style(Style::default().bg(PANEL).fg(TEXT))
}

fn draw_navigation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2 + LIST_TOP_PADDING) as usize;
    let scroll = app.nav_cursor.saturating_sub(height.saturating_sub(1));
    let lines = app
        .nav_rows
        .iter()
        .enumerate()
        .map(|(index, row)| match row {
            NavRow::Separator => Line::raw(""),
            NavRow::Item {
                name,
                count,
                is_inbox,
                ..
            } => {
                let selected = index == app.nav_cursor;
                let marker = if selected { "▸ " } else { "  " };
                let icon = if *is_inbox { "⌂ " } else { "" };
                Line::from(vec![
                    Span::styled(
                        format!("{marker}{icon}{name}"),
                        if selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(SELECT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        },
                    ),
                    Span::styled(
                        format!("  {count}"),
                        if selected {
                            Style::default().fg(Color::White).bg(SELECT)
                        } else {
                            Style::default().fg(MUTED)
                        },
                    ),
                ])
                .style(if selected {
                    Style::default().bg(SELECT)
                } else {
                    Style::default()
                })
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                panel(" Projects and Filters ", app.focus == Focus::Navigation)
                    .padding(Padding::new(0, 0, LIST_TOP_PADDING, 0)),
            )
            .scroll((scroll as u16, 0)),
        area,
    );
}

fn draw_tasks(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let content_width = area.width.saturating_sub(2 + TASK_RIGHT_PADDING).into();
    let (lines, positions) = task_lines(
        &app.tasks,
        app.task_cursor,
        app.focus == Focus::Tasks,
        app.group_tasks_by_label,
        app.show_completed_tasks,
        content_width,
    );
    let inner_height = area.height.saturating_sub(2 + LIST_TOP_PADDING) as usize;
    let scroll = task_scroll(&positions, app.task_cursor, inner_height);
    let title = format!(" {} ", app.selected_nav_name());
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                panel(Line::from(title), app.focus == Focus::Tasks).padding(Padding::new(
                    0,
                    TASK_RIGHT_PADDING,
                    LIST_TOP_PADDING,
                    0,
                )),
            )
            .scroll((scroll as u16, 0)),
        area,
    );
}

fn task_scroll(positions: &[usize], selected: Option<usize>, viewport_height: usize) -> usize {
    let Some(selected_index) = selected.filter(|index| *index < positions.len()) else {
        return 0;
    };
    if viewport_height == 0 {
        return positions[selected_index];
    }
    let selected_line = positions[selected_index];
    let lookahead_index = (selected_index + TASK_LOOKAHEAD).min(positions.len() - 1);
    let lookahead_line = positions[lookahead_index];
    // Put the fourth following task at or above the viewport's last row. If
    // headings make that impossible in a very short terminal, keep the
    // selected task visible and show as much lookahead as fits.
    lookahead_line
        .saturating_sub(viewport_height - 1)
        .min(selected_line)
}

fn task_lines<'a>(
    tasks: &'a [Task],
    selected: Option<usize>,
    focused: bool,
    group_by_label: bool,
    show_completed: bool,
    content_width: usize,
) -> (Vec<Line<'a>>, Vec<usize>) {
    if tasks.is_empty() {
        return (
            vec![Line::styled(
                if show_completed {
                    "  No tasks"
                } else {
                    "  No active tasks"
                },
                Style::default().fg(MUTED),
            )],
            Vec::new(),
        );
    }
    let mut lines = Vec::new();
    let mut positions = Vec::with_capacity(tasks.len());
    let mut previous: Option<Option<String>> = None;
    let today = Local::now().date_naive();
    for (index, task) in tasks.iter().enumerate() {
        if group_by_label {
            let group = task.display_group().map(str::to_owned);
            if previous.as_ref() != Some(&group) {
                if previous.is_some() {
                    lines.push(Line::raw(""));
                }
                if let Some(label) = group.as_deref() {
                    lines.push(Line::styled(
                        format!("  {label}"),
                        Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
                    ));
                }
                previous = Some(group);
            }
        }
        positions.push(lines.len());
        let is_selected = focused && selected == Some(index);
        let row_style = if is_selected {
            Style::default()
                .fg(Color::White)
                .bg(SELECT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let content_style = if task.checked {
            let style = Style::default()
                .fg(MUTED)
                .bg(if is_selected { SELECT } else { PANEL })
                .add_modifier(Modifier::CROSSED_OUT);
            if is_selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        } else {
            row_style
        };
        let priority = (task.priority >= 3).then(|| format!("  p{}", 5 - task.priority));
        let due = task
            .due
            .as_ref()
            .and_then(|due| due.list_display_on(today))
            .map(|due| format!("  {due}"));
        let suffix_width = priority.as_deref().map_or(0, UnicodeWidthStr::width)
            + due.as_deref().map_or(0, UnicodeWidthStr::width);
        let title_width = content_width.saturating_sub(2 + suffix_width);
        let visible_content = complete_words_within(&task.content, title_width);
        let mut spans = vec![
            Span::styled(if is_selected { "▸ " } else { "  " }, row_style),
            Span::styled(visible_content, content_style),
        ];
        if let Some(priority) = priority {
            spans.push(Span::styled(
                priority,
                Style::default()
                    .fg(if task.checked { MUTED } else { GOLD })
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        if let Some(due) = due {
            spans.push(Span::styled(
                due,
                Style::default()
                    .fg(if task.checked { MUTED } else { RED_BRIGHT })
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        lines.push(Line::from(spans).style(if is_selected {
            Style::default().bg(SELECT)
        } else {
            Style::default()
        }));
    }
    (lines, positions)
}

fn complete_words_within(content: &str, max_width: usize) -> &str {
    if UnicodeWidthStr::width(content) <= max_width {
        return content;
    }

    let mut width = 0;
    let mut inside_word = false;
    let mut last_complete_word_end = None;
    for (index, character) in content.char_indices() {
        if character.is_whitespace() {
            if inside_word {
                last_complete_word_end = Some(index);
                inside_word = false;
            }
        } else {
            inside_word = true;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_width {
            return last_complete_word_end
                .map(|end| content[..end].trim_end())
                .unwrap_or("");
        }
        width += character_width;
    }

    content
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let action_message = if let Some(prefix) = &app.prefix {
        format!("Jump: {prefix}")
    } else {
        app.status.clone()
    };
    let message = app.sync_issue.as_deref().unwrap_or(&action_message);
    let secondary_message = app
        .sync_issue
        .as_deref()
        .filter(|issue| *issue != action_message)
        .map(|_| action_message.as_str());
    let (symbol, sync_label, sync_color) = match app.sync_status {
        SyncStatus::Synced => (" ● ", "Synced", SYNC_GREEN),
        SyncStatus::Pending => (" ● ", "Pending", SYNC_ORANGE),
        SyncStatus::Offline => (" ● ", "Offline", MUTED),
        SyncStatus::Syncing => (" ● ", "Syncing", SYNC_ORANGE),
        SyncStatus::Error => (" ● ", "Error", SYNC_RED),
    };
    let redundant = matches!(
        (app.sync_status, message),
        (SyncStatus::Synced, "Synced") | (SyncStatus::Syncing, "Syncing…")
    );
    let mut spans = vec![
        Span::styled(
            symbol,
            Style::default()
                .fg(sync_color)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(sync_label, Style::default().fg(sync_color).bg(BG)),
    ];
    if !redundant && !message.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(MUTED).bg(BG)));
        spans.push(Span::styled(message, Style::default().fg(MUTED).bg(BG)));
    }
    if let Some(secondary) = secondary_message {
        spans.push(Span::styled(" · ", Style::default().fg(MUTED).bg(BG)));
        spans.push(Span::styled(secondary, Style::default().fg(TEXT).bg(BG)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_overlay(frame: &mut Frame<'_>, app: &mut App) {
    let keybindings = &app.keybindings;
    let config_path = app.config_path.to_string_lossy().into_owned();
    match app.overlay.as_mut() {
        Some(Overlay::Help) => draw_help(frame, keybindings, &config_path),
        Some(Overlay::Details(task)) => draw_details(frame, task),
        Some(Overlay::LabelPicker {
            query,
            cursor,
            mode,
            labels,
            ..
        }) => draw_label_picker(frame, query, *cursor, *mode, labels),
        Some(Overlay::ProjectPicker {
            query,
            cursor,
            mode,
            projects,
            ..
        }) => draw_project_picker(frame, query, *cursor, *mode, projects),
        Some(Overlay::Editor(editor)) => {
            let area = centered(frame.area(), 82, 18, 94, 70);
            let title = " Neovim — Enter accepts; :cq cancels ";
            let inner = panel(title, true).inner(area);
            editor.resize(inner.height, inner.width);
            frame.render_widget(Clear, area);
            frame.render_widget(panel(title, true), area);
            let lines = editor
                .lines()
                .into_iter()
                .map(Line::raw)
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines).style(Style::default().fg(TEXT).bg(PANEL)),
                inner,
            );
            let (row, col) = editor.cursor();
            if col < inner.width && row < inner.height {
                frame.set_cursor_position(Position::new(inner.x + col, inner.y + row));
            }
        }
        None => {}
    }
}

fn draw_help(frame: &mut Frame<'_>, keys: &KeyBindings, config_path: &str) {
    let area = centered(frame.area(), 80, 38, 96, 90);
    let mut help = vec![
        (
            format!(
                "{} / {}",
                keys.display(Action::Down),
                keys.display(Action::Up)
            ),
            "move down / up",
        ),
        (
            format!(
                "{} / {}",
                keys.display(Action::DownFive),
                keys.display(Action::UpFive)
            ),
            "move down / up five rows",
        ),
        (
            format!(
                "{} / {}",
                keys.display(Action::FocusNavigation),
                keys.display(Action::FocusTasks)
            ),
            "focus projects / tasks",
        ),
        (
            keys.display(Action::Confirm).into(),
            "enter tasks or show task details",
        ),
        (
            format!(
                "{} / {}",
                keys.display(Action::First),
                keys.display(Action::Last)
            ),
            "first / last row",
        ),
        (
            format!("{} + prefix", keys.display(Action::Jump)),
            "jump after at least two name characters",
        ),
        (
            keys.display(Action::AddTask).into(),
            "add task with Quick Add in Neovim",
        ),
        (
            keys.display(Action::EditTask).into(),
            "edit title and description",
        ),
        (
            keys.display(Action::CompleteTask).into(),
            "complete / reopen selected task",
        ),
        (keys.display(Action::Undo).into(), "undo last task change"),
        (keys.display(Action::Redo).into(), "redo last undone change"),
        (
            keys.display(Action::MoveTask).into(),
            "move task to another project",
        ),
        (
            keys.display(Action::EditDue).into(),
            "edit natural-language due date",
        ),
        (
            keys.display(Action::EditLabels).into(),
            "search and toggle a task label",
        ),
        (
            keys.display(Action::ToggleLabelGrouping).into(),
            "toggle label grouping",
        ),
        (
            keys.display(Action::ToggleAlphabeticalSort).into(),
            "toggle alphabetical task order",
        ),
        (
            keys.display(Action::ToggleCompletedTasks).into(),
            "show / hide completed tasks",
        ),
        (
            keys.display(Action::Sync).into(),
            "synchronize in background",
        ),
        (
            keys.display(Action::ToggleHiddenProjects).into(),
            "show / conceal hidden items",
        ),
        (
            keys.display(Action::ToggleProjectHidden).into(),
            "add / remove selected item from hidden group",
        ),
        (keys.display(Action::Help).into(), "open this help"),
        (keys.display(Action::Quit).into(), "quit application"),
        (
            keys.display(Action::Cancel).into(),
            "cancel prefix or change picker mode",
        ),
    ];
    const SPACE_HELP: [&str; 10] = [
        "assign/remove project space 0",
        "assign/remove project space 1",
        "assign/remove project space 2",
        "assign/remove project space 3",
        "assign/remove project space 4",
        "assign/remove project space 5",
        "assign/remove project space 6",
        "assign/remove project space 7",
        "assign/remove project space 8",
        "assign/remove project space 9",
    ];
    for (space, meaning) in SPACE_HELP.into_iter().enumerate() {
        let action = Action::for_space(space as u8).expect("space range is valid");
        help.push((keys.display(action).into(), meaning));
    }
    let mut lines = help
        .into_iter()
        .map(|(key, meaning)| {
            Line::from(vec![
                Span::styled(
                    format!("  {key:<14}"),
                    Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(meaning, Style::default().fg(TEXT)),
            ])
        })
        .collect::<Vec<_>>();
    for (key, project) in keys.special_add_bindings() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<14}"),
                Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("add task with #{project} as fallback"),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    for (prefix, project) in keys.project_jump_bindings() {
        let key = format!("{}{prefix}", keys.display(Action::Jump));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<14}"),
                Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("jump directly to #{project}"),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("  Config: {config_path}"),
        Style::default().fg(MUTED),
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(panel(" Keyboard shortcuts ", true)),
        area,
    );
}

fn draw_details(frame: &mut Frame<'_>, task: &Task) {
    let area = centered(frame.area(), 72, 18, 94, 82);
    let due = task.due.as_ref().and_then(|d| d.display()).unwrap_or("—");
    let labels = if task.labels.is_empty() {
        "—".into()
    } else {
        task.labels.join(", ")
    };
    let text = Text::from(vec![
        Line::styled(
            "Title",
            Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(task.content.clone()),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Due: ", Style::default().fg(MUTED)),
            Span::raw(due.to_owned()),
        ]),
        Line::from(vec![
            Span::styled("Priority: ", Style::default().fg(MUTED)),
            Span::raw(format!("p{}", 5 - task.priority)),
        ]),
        Line::from(vec![
            Span::styled("Labels: ", Style::default().fg(MUTED)),
            Span::raw(labels),
        ]),
        Line::raw(""),
        Line::styled(
            "Description",
            Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(if task.description.is_empty() {
            "—".into()
        } else {
            task.description.clone()
        }),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(" Task details ", true).padding(Padding::new(2, 2, 1, 1))),
        area,
    );
}

fn draw_label_picker(
    frame: &mut Frame<'_>,
    query: &str,
    cursor: usize,
    mode: PickerMode,
    labels: &[LabelPickerItem],
) {
    let area = centered(frame.area(), 62, 18, 76, 70);
    let title = match mode {
        PickerMode::Filtering => " Labels — type prefix; Esc selects ",
        PickerMode::Selecting => " Labels — j/k select; Esc cancels ",
    };
    let block = panel(title, true);
    let inner = block.inner(area);
    let matches = filtered_label_indices(labels, query);
    let list_height = inner.height.saturating_sub(2) as usize;
    let selected = cursor.min(matches.len().saturating_sub(1));
    let scroll = selected.saturating_sub(list_height.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "  > ",
                Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(query.to_owned(), Style::default().fg(TEXT)),
        ]),
        Line::raw(""),
    ];
    if matches.is_empty() {
        lines.push(Line::styled(
            "  No matching labels",
            Style::default().fg(MUTED),
        ));
    } else {
        lines.extend(
            matches
                .iter()
                .skip(scroll)
                .take(list_height)
                .enumerate()
                .filter_map(|(visible_index, label_index)| {
                    labels.get(*label_index).map(|label| {
                        let is_selected = scroll + visible_index == selected;
                        let style = if is_selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(SELECT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        };
                        Line::styled(
                            format!(
                                "{} [{}] {}",
                                if is_selected { "▸" } else { " " },
                                if label.selected { "✓" } else { " " },
                                label.name
                            ),
                            style,
                        )
                    })
                }),
        );
    }
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if mode == PickerMode::Filtering {
        let query_width = unicode_width::UnicodeWidthStr::width(query) as u16;
        frame.set_cursor_position(Position::new(
            (inner.x + 4 + query_width).min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }
}

fn draw_project_picker(
    frame: &mut Frame<'_>,
    query: &str,
    cursor: usize,
    mode: PickerMode,
    projects: &[ProjectPickerItem],
) {
    let area = centered(frame.area(), 62, 18, 76, 70);
    let title = match mode {
        PickerMode::Filtering => " Move task — type prefix; Esc selects ",
        PickerMode::Selecting => " Move task — j/k select; Esc cancels ",
    };
    let block = panel(title, true);
    let inner = block.inner(area);
    let matches = filtered_project_indices(projects, query);
    let list_height = inner.height.saturating_sub(2) as usize;
    let selected = cursor.min(matches.len().saturating_sub(1));
    let scroll = selected.saturating_sub(list_height.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "  > ",
                Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(query.to_owned(), Style::default().fg(TEXT)),
        ]),
        Line::raw(""),
    ];
    if matches.is_empty() {
        lines.push(Line::styled(
            "  No matching projects",
            Style::default().fg(MUTED),
        ));
    } else {
        lines.extend(
            matches
                .iter()
                .skip(scroll)
                .take(list_height)
                .enumerate()
                .filter_map(|(visible_index, project_index)| {
                    projects.get(*project_index).map(|project| {
                        let is_selected = scroll + visible_index == selected;
                        let style = if is_selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(SELECT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        };
                        Line::styled(
                            format!("{} {}", if is_selected { "▸" } else { " " }, project.name),
                            style,
                        )
                    })
                }),
        );
    }
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if mode == PickerMode::Filtering {
        let query_width = unicode_width::UnicodeWidthStr::width(query) as u16;
        frame.set_cursor_position(Position::new(
            (inner.x + 4 + query_width).min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }
}

fn centered(
    parent: Rect,
    width_percent: u16,
    height: u16,
    max_width: u16,
    max_height_percent: u16,
) -> Rect {
    let width = (parent.width * width_percent / 100)
        .min(max_width)
        .max(20)
        .min(parent.width);
    let max_height = parent.height * max_height_percent / 100;
    let height = height.min(max_height).max(3).min(parent.height);
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(parent);
    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(content: &str, labels: &[&str]) -> Task {
        Task {
            id: content.into(),
            project_id: "inbox".into(),
            content: content.into(),
            description: String::new(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
        }
    }

    #[test]
    fn task_rows_are_compact_and_keep_heading_attached() {
        let tasks = vec![task("First", &["Work"]), task("Second", &["Work"])];

        let (lines, positions) = task_lines(&tasks, Some(0), true, true, false, 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(positions, [1, 2]);
    }

    #[test]
    fn ungrouped_task_rows_have_no_headings_or_separators() {
        let tasks = vec![task("First", &["Work"]), task("Second", &["Home"])];

        let (lines, positions) = task_lines(&tasks, Some(0), true, false, false, 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(positions, [0, 1]);
    }

    #[test]
    fn completed_task_titles_are_muted_and_struck_through() {
        let mut completed = task("Finished", &[]);
        completed.checked = true;

        let tasks = [completed];
        let (lines, _) = task_lines(&tasks, Some(0), true, false, true, 80);
        let style = lines[0].spans[1].style;
        assert_eq!(style.fg, Some(MUTED));
        assert!(style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(style.bg, Some(SELECT));
    }

    #[test]
    fn task_titles_end_at_complete_word_boundaries() {
        let tasks = [task("Alpha beta gamma", &[])];

        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, 15);

        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha beta");
    }

    #[test]
    fn task_title_width_reserves_room_for_metadata() {
        let mut important = task("Alpha beta", &[]);
        important.priority = 4;
        let tasks = [important];

        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, 15);

        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha");
        assert_eq!(lines[0].spans[2].content.as_ref(), "  p1");
    }

    #[test]
    fn oversized_first_word_is_not_rendered_partially() {
        assert_eq!(complete_words_within("extraordinary", 6), "");
    }

    #[test]
    fn task_scroll_keeps_four_following_tasks_visible() {
        let positions = (0..12).collect::<Vec<_>>();
        assert_eq!(task_scroll(&positions, Some(2), 7), 0);
        assert_eq!(task_scroll(&positions, Some(3), 7), 1);
        assert_eq!(task_scroll(&positions, Some(7), 7), 5);
    }

    #[test]
    fn task_scroll_accounts_for_group_heading_rows() {
        let positions = vec![0, 1, 4, 5, 8, 9, 12];
        assert_eq!(task_scroll(&positions, Some(0), 9), 0);
        assert_eq!(task_scroll(&positions, Some(1), 9), 1);
    }

    #[test]
    fn task_scroll_keeps_selection_visible_in_tiny_viewport() {
        let positions = vec![0, 3, 6, 9, 12];
        assert_eq!(task_scroll(&positions, Some(0), 5), 0);
    }
}

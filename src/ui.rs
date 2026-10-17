use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Local, NaiveDate};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{
        App, FilterPickerItem, Focus, LabelPickerItem, Overlay, PickerMode, ProjectPickerItem,
        ProjectPickerPurpose, SyncStatus, TaskPickerItem, filtered_filter_indices,
        filtered_label_indices, filtered_project_indices, filtered_task_indices,
    },
    editor::EditorCellStyle,
    keymap::{Action, KeyBindings},
    model::{Filter, NavId, NavRow, Task},
};

const BG: Color = Color::Rgb(20, 20, 22);
const PANEL: Color = Color::Rgb(27, 27, 30);
const RED: Color = Color::Rgb(164, 49, 61);
const RED_BRIGHT: Color = Color::Rgb(211, 84, 96);
const SELECT: Color = Color::Rgb(91, 34, 42);
const INACTIVE_SELECT: Color = Color::Rgb(57, 57, 63);
const TEXT: Color = Color::Rgb(226, 222, 220);
const MUTED: Color = Color::Rgb(139, 134, 132);
const GOLD: Color = Color::Rgb(209, 166, 73);
const SYNC_GREEN: Color = Color::Rgb(76, 187, 123);
const SYNC_ORANGE: Color = Color::Rgb(224, 151, 62);
const SYNC_RED: Color = Color::Rgb(222, 76, 86);
const TASK_LOOKAHEAD: usize = 4;
const LIST_TOP_PADDING: u16 = 1;
const TASK_RIGHT_PADDING: u16 = 2;
const DESCRIPTION_MARKER: &str = "\u{2630}";

fn priority_display(priority: u8) -> (&'static str, Option<Color>) {
    match priority {
        4 => ("p1", Some(RED_BRIGHT)),
        3 => ("p2", Some(SYNC_ORANGE)),
        2 => ("p3", Some(GOLD)),
        _ => ("p4", None),
    }
}

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
    let selection_color = navigation_selection_color(app.focus);
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
                depth,
                ..
            } => {
                let selected = index == app.nav_cursor;
                let marker = if selected { "▸ " } else { "  " };
                let icon = if *is_inbox { "⌂ " } else { "" };
                Line::from(vec![
                    Span::styled(
                        format!("{marker}{icon}{}{name}", "  ".repeat(*depth)),
                        if selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(selection_color)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        },
                    ),
                    Span::styled(
                        format!("  {count}"),
                        if selected {
                            Style::default().fg(Color::White).bg(selection_color)
                        } else {
                            Style::default().fg(MUTED)
                        },
                    ),
                ])
                .style(if selected {
                    Style::default().bg(selection_color)
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

fn navigation_selection_color(focus: Focus) -> Color {
    match focus {
        Focus::Navigation => SELECT,
        Focus::Tasks => INACTIVE_SELECT,
    }
}

fn draw_tasks(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let content_width = area.width.saturating_sub(2 + TASK_RIGHT_PADDING).into();
    let group_by_date = matches!(app.selected_nav(), Some(NavId::Upcoming));
    let (lines, positions) = task_lines(
        &app.tasks,
        app.task_cursor,
        app.focus == Focus::Tasks,
        app.group_tasks_by_label && !group_by_date,
        group_by_date,
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
    group_by_date: bool,
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
    let task_depths = task_depths(tasks);
    for (index, task) in tasks.iter().enumerate() {
        let depth = task_depths[index];
        if (group_by_label || group_by_date) && depth == 0 {
            let group = if group_by_date {
                task.due
                    .as_ref()
                    .and_then(crate::model::Due::concrete_date)
                    .map(|date| upcoming_date_heading(date, today))
            } else {
                task.display_group().map(str::to_owned)
            };
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
        let (priority_label, priority_color) = priority_display(task.priority);
        let priority = priority_color.map(|color| (format!("  {priority_label}"), color));
        let due = (!group_by_date)
            .then(|| {
                task.due
                    .as_ref()
                    .and_then(|due| due.list_display_on(today))
                    .map(|due| {
                        let color = if due == "Tod" || due.starts_with("Tod ") {
                            RED_BRIGHT
                        } else {
                            SYNC_ORANGE
                        };
                        (format!("  {due}"), color)
                    })
            })
            .flatten();
        let assignee = task.assignee.as_ref().map(|name| format!("  @{name}"));
        let description = (!task.description.is_empty()).then(|| format!("  {DESCRIPTION_MARKER}"));
        let suffix_width = priority
            .as_ref()
            .map_or(0, |(label, _)| UnicodeWidthStr::width(label.as_str()))
            + due
                .as_ref()
                .map_or(0, |(label, _)| UnicodeWidthStr::width(label.as_str()))
            + assignee.as_deref().map_or(0, UnicodeWidthStr::width)
            + description.as_deref().map_or(0, UnicodeWidthStr::width);
        let indentation = depth.saturating_mul(2);
        let title_width = content_width.saturating_sub(2 + indentation + suffix_width);
        let continuation_width = content_width.saturating_sub(3 + indentation);
        let wrapped_content = wrap_complete_words(&task.content, title_width, continuation_width);
        let mut spans = vec![Span::styled(
            if is_selected { "▸ " } else { "  " },
            row_style,
        )];
        if depth > 0 {
            spans.push(Span::styled(
                format!("{}○ ", "  ".repeat(depth - 1)),
                Style::default()
                    .fg(RED_BRIGHT)
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        spans.push(Span::styled(
            wrapped_content.first().cloned().unwrap_or_default(),
            content_style,
        ));
        if let Some((priority, color)) = priority {
            spans.push(Span::styled(
                priority,
                Style::default()
                    .fg(if task.checked { MUTED } else { color })
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        if let Some((due, color)) = due {
            spans.push(Span::styled(
                due,
                Style::default()
                    .fg(if task.checked { MUTED } else { color })
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        if let Some(assignee) = assignee {
            spans.push(Span::styled(
                assignee,
                Style::default()
                    .fg(MUTED)
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        if let Some(description) = description {
            spans.push(Span::styled(
                description,
                Style::default()
                    .fg(MUTED)
                    .bg(if is_selected { SELECT } else { PANEL }),
            ));
        }
        let line_style = if is_selected {
            Style::default().bg(SELECT)
        } else {
            Style::default()
        };
        lines.push(Line::from(spans).style(line_style));
        let continuation_indent = " ".repeat(3 + indentation);
        for content in wrapped_content.into_iter().skip(1) {
            lines.push(
                Line::from(vec![
                    Span::styled(continuation_indent.clone(), row_style),
                    Span::styled(content, content_style),
                ])
                .style(line_style),
            );
        }
    }
    (lines, positions)
}

fn task_depths(tasks: &[Task]) -> Vec<usize> {
    let by_id = tasks
        .iter()
        .map(|task| (task.id.as_str(), task))
        .collect::<HashMap<_, _>>();
    tasks
        .iter()
        .map(|task| {
            let mut depth = 0;
            let mut parent_id = task.parent_id.as_deref();
            let mut seen = HashSet::new();
            while let Some(parent) = parent_id {
                if parent == task.id || !seen.insert(parent) {
                    break;
                }
                let Some(parent_task) = by_id.get(parent) else {
                    break;
                };
                depth += 1;
                parent_id = parent_task.parent_id.as_deref();
            }
            depth
        })
        .collect()
}

fn upcoming_date_heading(date: NaiveDate, today: NaiveDate) -> String {
    if date.year() == today.year() {
        date.format("%a, %b %-d").to_string()
    } else {
        date.format("%a, %b %-d, %Y").to_string()
    }
}

fn wrap_complete_words(
    content: &str,
    first_line_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut width_limit = first_line_width;

    for word in content.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        let separator_width = usize::from(!current.is_empty());
        if UnicodeWidthStr::width(current.as_str()) + separator_width + word_width <= width_limit {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }

        if current.is_empty() {
            // There is no way to display this word without splitting it.
            // Keep the row intact instead of allowing the terminal to clip a
            // partial word at the border.
            break;
        }

        lines.push(std::mem::take(&mut current));
        width_limit = continuation_width;
        if word_width > width_limit {
            break;
        }
        current.push_str(word);
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn complete_words_within(content: &str, max_width: usize) -> String {
    wrap_complete_words(content, max_width, max_width)
        .into_iter()
        .next()
        .unwrap_or_default()
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
        Some(Overlay::Help { scroll }) => draw_help(frame, keybindings, &config_path, scroll),
        Some(Overlay::ErrorDetails { message, scroll }) => {
            draw_error_details(frame, message, *scroll)
        }
        Some(Overlay::Details(task)) => draw_details(
            frame,
            task,
            keybindings.display(Action::EditTask),
            keybindings.display(Action::EditDue),
            keybindings.display(Action::RemoveDue),
            keybindings.display(Action::DeleteTask),
        ),
        Some(Overlay::FilterDetails(filter)) => {
            draw_filter_details(frame, filter, keybindings.display(Action::EditTask))
        }
        Some(Overlay::LabelPicker {
            query,
            cursor,
            mode,
            labels,
            ..
        }) => draw_label_picker(frame, query, *cursor, *mode, labels),
        Some(Overlay::ProjectPicker {
            purpose,
            query,
            cursor,
            mode,
            projects,
            ..
        }) => draw_project_picker(frame, purpose, query, *cursor, *mode, projects),
        Some(Overlay::TaskPicker {
            query,
            cursor,
            mode,
            tasks,
        }) => draw_task_picker(frame, query, *cursor, *mode, tasks),
        Some(Overlay::FilterPicker {
            query,
            cursor,
            mode,
            filters,
        }) => draw_filter_picker(frame, query, *cursor, *mode, filters),
        Some(Overlay::Editor(editor)) => {
            let area = centered(frame.area(), 82, 18, 94, 70);
            let title = match (editor.edits_filter(), editor.save_on_enter_in_insert()) {
                (true, true) => " Neovim — arrows: fields; Enter saves; :cq cancels ",
                (true, false) => " Neovim — arrows: fields; Normal Enter saves; :cq cancels ",
                (false, true) => " Neovim — Enter saves; Shift+Enter newline; :cq cancels ",
                (false, false) => " Neovim — Normal Enter saves; Shift+Enter newline; :cq cancels ",
            };
            let inner = panel(title, true).inner(area);
            editor.resize(inner.height, inner.width);
            frame.render_widget(Clear, area);
            frame.render_widget(panel(title, true), area);
            let lines = editor
                .styled_lines()
                .into_iter()
                .map(|spans| {
                    Line::from(
                        spans
                            .into_iter()
                            .map(|span| Span::styled(span.text, editor_cell_style(span.style)))
                            .collect::<Vec<_>>(),
                    )
                })
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

fn draw_error_details(frame: &mut Frame<'_>, message: &str, scroll: usize) {
    let area = centered(frame.area(), 86, 16, 96, 82);
    let block = panel(" Error details — j/k scroll; Enter/Esc/q closes ", true)
        .padding(Padding::new(2, 2, 1, 1));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0))
            .block(block),
        area,
    );
}

fn editor_cell_style(cell: EditorCellStyle) -> Style {
    let mut foreground = editor_color(cell.foreground, TEXT);
    let mut background = editor_color(cell.background, PANEL);
    if cell.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    let mut modifiers = Modifier::empty();
    if cell.bold {
        modifiers.insert(Modifier::BOLD);
    }
    if cell.dim {
        modifiers.insert(Modifier::DIM);
    }
    if cell.italic {
        modifiers.insert(Modifier::ITALIC);
    }
    if cell.underline {
        modifiers.insert(Modifier::UNDERLINED);
    }
    Style::default()
        .fg(foreground)
        .bg(background)
        .add_modifier(modifiers)
}

fn editor_color(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn draw_help(frame: &mut Frame<'_>, keys: &KeyBindings, config_path: &str, scroll: &mut usize) {
    let area = centered(frame.area(), 80, 44, 96, 94);
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
            keys.display(Action::SearchProjects).into(),
            "search all projects, including hidden projects",
        ),
        (
            keys.display(Action::SearchTasks).into(),
            "search all cached tasks",
        ),
        (
            keys.display(Action::SearchFilters).into(),
            "search all cached filters",
        ),
        (
            keys.display(Action::AddFilter).into(),
            "add a personal filter in Neovim",
        ),
        (
            keys.display(Action::RenameProject).into(),
            "rename selected project (sync elsewhere)",
        ),
        (
            keys.display(Action::AddProject).into(),
            "add a personal project in Neovim",
        ),
        (
            format!(
                "{} / {}",
                keys.display(Action::MoveProjectDown),
                keys.display(Action::MoveProjectUp)
            ),
            "move selected project or task down / up among siblings",
        ),
        (
            keys.display(Action::CopyTitle).into(),
            "copy selected project/filter or task title",
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
            keys.display(Action::AddTaskTop).into(),
            "add task at top in manual order",
        ),
        (
            keys.display(Action::AddTaskBottom).into(),
            "add task at bottom in manual order",
        ),
        (
            keys.display(Action::AddChildTask).into(),
            "add subtask to selected task",
        ),
        (
            keys.display(Action::EditTask).into(),
            "edit title and description",
        ),
        (
            keys.display(Action::CompleteTask).into(),
            "complete / reopen selected task",
        ),
        (
            keys.display(Action::DeleteTask).into(),
            "delete selected task and its subtasks",
        ),
        (keys.display(Action::Undo).into(), "undo last task change"),
        (keys.display(Action::Redo).into(), "redo last undone change"),
        (
            keys.display(Action::MoveTask).into(),
            "move task to another project",
        ),
        (
            format!(
                "{} / {}",
                keys.display(Action::IndentTask),
                keys.display(Action::OutdentTask)
            ),
            "indent under previous sibling / outdent one level",
        ),
        (
            keys.display(Action::EditDue).into(),
            "edit natural-language due date",
        ),
        (
            keys.display(Action::RemoveDue).into(),
            "remove task due date",
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
            keys.display(Action::ShowError).into(),
            "show complete synchronization error",
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
        (
            keys.display(Action::CloseDialog).into(),
            "close help, task details, or filter details",
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
    help.retain(|(key, _)| {
        key.chars()
            .any(|character| character != '—' && character != '/' && !character.is_whitespace())
    });
    let mut lines = help
        .into_iter()
        .map(|(key, meaning)| {
            Line::from(vec![
                Span::styled(
                    format!("  {key:<14}        "),
                    Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(meaning, Style::default().fg(TEXT)),
            ])
        })
        .collect::<Vec<_>>();
    for (key, project) in keys.special_add_bindings() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<14}        "),
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
                format!("  {key:<14}        "),
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
    let title = format!(
        " Keyboard shortcuts — {}/{} scroll ",
        keys.display(Action::Down),
        keys.display(Action::Up)
    );
    let block = panel(title, true);
    let viewport_height = block.inner(area).height as usize;
    *scroll = (*scroll).min(lines.len().saturating_sub(viewport_height));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll(((*scroll).min(u16::MAX as usize) as u16, 0))
            .block(block),
        area,
    );
}

fn draw_details(
    frame: &mut Frame<'_>,
    task: &Task,
    edit_key: &str,
    due_key: &str,
    remove_due_key: &str,
    delete_key: &str,
) {
    let area = centered(frame.area(), 72, 18, 94, 82);
    let due = task
        .due
        .as_ref()
        .and_then(|d| d.readable_display())
        .unwrap_or_else(|| "—".into());
    let labels = if task.labels.is_empty() {
        "—".into()
    } else {
        task.labels.join(", ")
    };
    let assignee = task.assignee.as_deref().unwrap_or("—");
    let (priority_label, priority_color) = priority_display(task.priority);
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
            Span::styled(
                priority_label,
                Style::default().fg(priority_color.unwrap_or(TEXT)),
            ),
        ]),
        Line::from(vec![
            Span::styled("Labels: ", Style::default().fg(MUTED)),
            Span::raw(labels),
        ]),
        Line::from(vec![
            Span::styled("Assignee: ", Style::default().fg(MUTED)),
            Span::raw(assignee.to_owned()),
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
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            panel(
                format!(
                    " Task details — {edit_key} edit; {due_key} date; {remove_due_key} clear; {delete_key} delete "
                ),
                true,
            )
            .padding(Padding::new(2, 2, 1, 1)),
        ),
        area,
    );
}

fn draw_filter_details(frame: &mut Frame<'_>, filter: &Filter, edit_key: &str) {
    let area = centered(frame.area(), 72, 14, 94, 72);
    let scope = if filter.workspace_id.is_some() {
        "Workspace"
    } else {
        "Personal"
    };
    let text = Text::from(vec![
        Line::styled(
            "Title",
            Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(filter.name.clone()),
        Line::raw(""),
        Line::styled(
            "Query",
            Style::default().fg(RED_BRIGHT).add_modifier(Modifier::BOLD),
        ),
        Line::raw(filter.query.clone()),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Favorite: ", Style::default().fg(MUTED)),
            Span::raw(if filter.is_favorite { "Yes" } else { "No" }),
        ]),
        Line::from(vec![
            Span::styled("Scope: ", Style::default().fg(MUTED)),
            Span::raw(scope),
        ]),
        Line::from(vec![
            Span::styled("Access: ", Style::default().fg(MUTED)),
            Span::raw(if filter.is_frozen {
                "Read-only"
            } else {
                "Editable"
            }),
        ]),
    ]);
    frame.render_widget(Clear, area);
    let title = if filter.is_frozen {
        " Filter details — read-only ".to_owned()
    } else {
        format!(" Filter details — {edit_key} edits ")
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(title, true).padding(Padding::new(2, 2, 1, 1))),
        area,
    );
}

fn draw_task_picker(
    frame: &mut Frame<'_>,
    query: &str,
    cursor: usize,
    mode: PickerMode,
    tasks: &[TaskPickerItem],
) {
    let area = centered(frame.area(), 72, 20, 94, 76);
    let title = match mode {
        PickerMode::Filtering => " Search tasks — type prefix; Esc selects ",
        PickerMode::Selecting => " Search tasks — j/k select; Esc cancels ",
    };
    let block = panel(title, true);
    let inner = block.inner(area);
    let matches = filtered_task_indices(tasks, query);
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
            "  No matching tasks",
            Style::default().fg(MUTED),
        ));
    } else {
        let content_width = inner.width.saturating_sub(3) as usize;
        lines.extend(
            matches
                .iter()
                .skip(scroll)
                .take(list_height)
                .enumerate()
                .filter_map(|(visible_index, task_index)| {
                    tasks.get(*task_index).map(|item| {
                        let is_selected = scroll + visible_index == selected;
                        let row_style = if is_selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(SELECT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        };
                        let candidate_suffix = format!("  · {}", item.project_name);
                        let suffix = if UnicodeWidthStr::width(candidate_suffix.as_str())
                            <= content_width / 3
                        {
                            candidate_suffix
                        } else {
                            String::new()
                        };
                        let title_width =
                            content_width.saturating_sub(UnicodeWidthStr::width(suffix.as_str()));
                        let task_style = if item.task.checked {
                            row_style.fg(MUTED).add_modifier(Modifier::CROSSED_OUT)
                        } else {
                            row_style
                        };
                        Line::from(vec![
                            Span::styled(if is_selected { "▸ " } else { "  " }, row_style),
                            Span::styled(
                                complete_words_within(&item.task.content, title_width),
                                task_style,
                            ),
                            Span::styled(suffix, row_style.fg(MUTED)),
                        ])
                        .style(if is_selected {
                            Style::default().bg(SELECT)
                        } else {
                            Style::default()
                        })
                    })
                }),
        );
    }
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if mode == PickerMode::Filtering {
        let query_width = UnicodeWidthStr::width(query) as u16;
        frame.set_cursor_position(Position::new(
            (inner.x + 4 + query_width).min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }
}

fn draw_filter_picker(
    frame: &mut Frame<'_>,
    query: &str,
    cursor: usize,
    mode: PickerMode,
    filters: &[FilterPickerItem],
) {
    let area = centered(frame.area(), 68, 18, 88, 72);
    let title = match mode {
        PickerMode::Filtering => " Search filters — type prefix; Esc selects ",
        PickerMode::Selecting => " Search filters — j/k select; Esc cancels ",
    };
    let block = panel(title, true);
    let inner = block.inner(area);
    let matches = filtered_filter_indices(filters, query);
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
            "  No matching filters",
            Style::default().fg(MUTED),
        ));
    } else {
        lines.extend(
            matches
                .iter()
                .skip(scroll)
                .take(list_height)
                .enumerate()
                .filter_map(|(visible_index, filter_index)| {
                    filters.get(*filter_index).map(|item| {
                        let is_selected = scroll + visible_index == selected;
                        let style = if is_selected {
                            Style::default()
                                .fg(Color::White)
                                .bg(SELECT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(TEXT)
                        };
                        let suffix = if item.filter.is_favorite { "  ★" } else { "" };
                        Line::styled(
                            format!(
                                "{} {}{}",
                                if is_selected { "▸" } else { " " },
                                item.filter.name,
                                suffix
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
        let query_width = UnicodeWidthStr::width(query) as u16;
        frame.set_cursor_position(Position::new(
            (inner.x + 4 + query_width).min(inner.right().saturating_sub(1)),
            inner.y,
        ));
    }
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
    purpose: &ProjectPickerPurpose,
    query: &str,
    cursor: usize,
    mode: PickerMode,
    projects: &[ProjectPickerItem],
) {
    let area = centered(frame.area(), 62, 18, 76, 70);
    let action = match purpose {
        ProjectPickerPurpose::Jump => "Find project",
        ProjectPickerPurpose::MoveTask { .. } => "Move task",
    };
    let title = match mode {
        PickerMode::Filtering => format!(" {action} — type prefix; Esc selects "),
        PickerMode::Selecting => format!(" {action} — j/k select; Esc cancels "),
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
            parent_id: None,
            content: content.into(),
            description: String::new(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            assignee: None,
        }
    }

    #[test]
    fn navigation_selection_is_red_when_focused_and_grey_when_tasks_are_focused() {
        assert_eq!(navigation_selection_color(Focus::Navigation), SELECT);
        assert_eq!(navigation_selection_color(Focus::Tasks), INACTIVE_SELECT);
    }

    #[test]
    fn embedded_editor_inverse_video_swaps_default_colors() {
        let style = editor_cell_style(EditorCellStyle {
            foreground: vt100::Color::Default,
            background: vt100::Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: true,
        });

        assert_eq!(style.fg, Some(PANEL));
        assert_eq!(style.bg, Some(TEXT));
    }

    #[test]
    fn task_rows_are_compact_and_keep_heading_attached() {
        let tasks = vec![task("First", &["Work"]), task("Second", &["Work"])];

        let (lines, positions) = task_lines(&tasks, Some(0), true, true, false, false, 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(positions, [1, 2]);
    }

    #[test]
    fn task_description_shows_a_text_marker_on_the_right() {
        let mut with_description = task("With note", &[]);
        with_description.description = "Body".into();
        let tasks = [task("Plain", &[]), with_description];

        let (lines, _) = task_lines(&tasks, None, false, false, false, false, 80);

        assert!(
            !lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains(DESCRIPTION_MARKER))
        );
        let marker = format!("  {DESCRIPTION_MARKER}");
        assert_eq!(
            lines[1].spans.last().map(|span| span.content.as_ref()),
            Some(marker.as_str())
        );
    }

    #[test]
    fn child_tasks_are_indented_and_do_not_create_label_groups() {
        let parent = task("Parent", &["Work"]);
        let mut child = task("Child", &["Personal"]);
        child.parent_id = Some(parent.id.clone());
        let tasks = [parent, child];

        let (lines, positions) = task_lines(&tasks, Some(1), true, true, false, false, 80);

        assert_eq!(lines.len(), 3);
        assert_eq!(positions, [1, 2]);
        assert_eq!(lines[0].spans[0].content.as_ref(), "  Work");
        assert_eq!(lines[2].spans[0].content.as_ref(), "▸ ");
        assert_eq!(lines[2].spans[1].content.as_ref(), "○ ");
        assert_eq!(lines[2].spans[1].style.fg, Some(RED_BRIGHT));
        assert_eq!(lines[2].spans[2].content.as_ref(), "Child");
        let prefix_width: usize = lines[2].spans[..2]
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        assert_eq!(prefix_width, 4);
    }

    #[test]
    fn upcoming_tasks_have_date_headings_without_repeated_due_suffixes() {
        let today = Local::now().date_naive();
        let mut first = task("First", &[]);
        first.due = Some(crate::model::Due {
            date: Some(today.format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let mut second = task("Second", &[]);
        second.due = Some(crate::model::Due {
            date: Some((today + chrono::Duration::days(1)).format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let tasks = [first, second];

        let (lines, positions) = task_lines(&tasks, Some(0), true, false, true, false, 80);

        assert_eq!(positions, [1, 4]);
        assert_eq!(
            lines[0].spans[0].content.as_ref(),
            format!("  {}", upcoming_date_heading(today, today))
        );
        assert!(lines[2].spans.is_empty());
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[4].spans.len(), 2);
    }

    #[test]
    fn ungrouped_task_rows_have_no_headings_or_separators() {
        let tasks = vec![task("First", &["Work"]), task("Second", &["Home"])];

        let (lines, positions) = task_lines(&tasks, Some(0), true, false, false, false, 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(positions, [0, 1]);
    }

    #[test]
    fn completed_task_titles_are_muted_and_struck_through() {
        let mut completed = task("Finished", &[]);
        completed.checked = true;

        let tasks = [completed];
        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, true, 80);
        let style = lines[0].spans[1].style;
        assert_eq!(style.fg, Some(MUTED));
        assert!(style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(style.bg, Some(SELECT));
    }

    #[test]
    fn task_titles_end_at_complete_word_boundaries() {
        let tasks = [task("Alpha beta gamma", &[])];

        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, false, 15);

        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha beta");
        assert_eq!(lines[1].spans[0].content.as_ref(), "   ");
        assert_eq!(lines[1].spans[1].content.as_ref(), "gamma");
        assert_eq!(lines[1].style.bg, Some(SELECT));
    }

    #[test]
    fn wrapped_task_lines_are_indented_and_shift_following_task_positions() {
        let tasks = [task("Alpha beta gamma delta", &[]), task("Next", &[])];

        let (lines, positions) = task_lines(&tasks, Some(0), true, false, false, false, 12);

        assert_eq!(positions, [0, 3]);
        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha beta");
        assert_eq!(lines[1].spans[0].content.as_ref(), "   ");
        assert_eq!(lines[1].spans[1].content.as_ref(), "gamma");
        assert_eq!(lines[2].spans[0].content.as_ref(), "   ");
        assert_eq!(lines[2].spans[1].content.as_ref(), "delta");
        assert_eq!(lines[3].spans[1].content.as_ref(), "Next");
    }

    #[test]
    fn task_title_width_reserves_room_for_metadata() {
        let mut important = task("Alpha beta", &[]);
        important.priority = 4;
        let tasks = [important];

        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, false, 15);

        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha");
        assert_eq!(lines[0].spans[2].content.as_ref(), "  p1");
    }

    #[test]
    fn assigned_tasks_show_the_assignee_and_reserve_title_width() {
        let mut assigned = task("Alpha beta", &[]);
        assigned.assignee = Some("Ada Lovelace".into());
        let tasks = [assigned];

        let (lines, _) = task_lines(&tasks, Some(0), true, false, false, false, 23);

        assert_eq!(lines[0].spans[1].content.as_ref(), "Alpha");
        assert_eq!(lines[0].spans[2].content.as_ref(), "  @Ada Lovelace");
        assert_eq!(lines[0].spans[2].style.fg, Some(MUTED));
    }

    #[test]
    fn task_due_dates_are_orange_except_for_today() {
        let today = Local::now().date_naive();
        let mut today_task = task("Today", &[]);
        today_task.due = Some(crate::model::Due {
            date: Some(today.format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let mut tomorrow_task = task("Tomorrow", &[]);
        tomorrow_task.due = Some(crate::model::Due {
            date: Some((today + chrono::Duration::days(1)).format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let tasks = [today_task, tomorrow_task];

        let (lines, _) = task_lines(&tasks, None, true, false, false, false, 80);

        assert_eq!(lines[0].spans[2].content.as_ref(), "  Tod");
        assert_eq!(lines[0].spans[2].style.fg, Some(RED_BRIGHT));
        assert_eq!(lines[1].spans[2].style.fg, Some(SYNC_ORANGE));
    }

    #[test]
    fn task_priorities_use_todoist_labels_and_distinct_colors() {
        for (priority, label, color) in [
            (4, "  p1", RED_BRIGHT),
            (3, "  p2", SYNC_ORANGE),
            (2, "  p3", GOLD),
        ] {
            let mut item = task("Important", &[]);
            item.priority = priority;
            let tasks = [item];

            let (lines, _) = task_lines(&tasks, Some(0), true, false, false, false, 80);

            assert_eq!(lines[0].spans[2].content.as_ref(), label);
            assert_eq!(lines[0].spans[2].style.fg, Some(color));
        }
    }

    #[test]
    fn oversized_first_word_is_not_rendered_partially() {
        assert_eq!(wrap_complete_words("extraordinary", 6, 5), [""]);
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

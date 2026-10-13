use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    editor::{EditorCursorShape, EditorPurpose, EditorResult, EmbeddedEditor},
    keymap::{Action, KeyBindings},
    model::{NavId, NavRow, Project, Task},
    store::{Store, TaskViewPreferences},
    sync::{SyncEvent, SyncFailureKind, SyncManager},
};

const NAV_DEBOUNCE: Duration = Duration::from_millis(50);
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5);
const EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(16);
const SYNC_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Navigation,
    Tasks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Synced,
    Pending,
    Offline,
    Syncing,
    Error,
}

impl SyncStatus {
    fn mark_local_change(&mut self) {
        if *self == Self::Synced {
            *self = Self::Pending;
        }
    }
}

pub enum Overlay {
    Help,
    Details(Task),
    LabelPicker {
        task_id: String,
        query: String,
        cursor: usize,
        mode: PickerMode,
        labels: Vec<LabelPickerItem>,
        task_labels: Vec<String>,
    },
    ProjectPicker {
        task_id: String,
        query: String,
        cursor: usize,
        mode: PickerMode,
        projects: Vec<ProjectPickerItem>,
    },
    Editor(EmbeddedEditor),
}

#[derive(Debug, Clone)]
pub struct ProjectPickerItem {
    pub id: String,
    pub name: String,
    search_name: String,
}

#[derive(Debug, Clone)]
pub struct LabelPickerItem {
    pub name: String,
    pub selected: bool,
    search_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Filtering,
    Selecting,
}

#[derive(Debug, Clone)]
enum HistoryEntry {
    TaskChange {
        before: Box<Task>,
        after: Box<Task>,
        action: &'static str,
    },
    TaskAdd {
        expression: String,
        project: Project,
        task_id: String,
    },
}

impl HistoryEntry {
    fn action(&self) -> &'static str {
        match self {
            Self::TaskChange { action, .. } => action,
            Self::TaskAdd { .. } => "task addition",
        }
    }
}

impl PickerMode {
    fn cancel(&mut self) -> bool {
        match self {
            Self::Filtering => {
                *self = Self::Selecting;
                false
            }
            Self::Selecting => true,
        }
    }
}

#[derive(Debug)]
struct NavSearchEntry {
    row_index: usize,
    normalized_name: String,
    is_project: bool,
    uses_custom_jump: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavSearchMatch {
    None,
    Unique(usize),
    Ambiguous,
}

pub struct App {
    pub store: Store,
    pub sync: SyncManager,
    pub nav_rows: Vec<NavRow>,
    pub nav_cursor: usize,
    pub tasks: Vec<Task>,
    pub task_cursor: Option<usize>,
    pub focus: Focus,
    pub status: String,
    pub sync_status: SyncStatus,
    pub sync_issue: Option<String>,
    pub overlay: Option<Overlay>,
    pub prefix: Option<String>,
    pub quit: bool,
    pub show_hidden_projects: bool,
    pub group_tasks_by_label: bool,
    pub sort_tasks_alphabetically: bool,
    pub show_completed_tasks: bool,
    pub keybindings: KeyBindings,
    pub config_path: std::path::PathBuf,
    nav_search: Vec<NavSearchEntry>,
    remembered_tasks: HashMap<NavId, String>,
    rendered_nav: Option<NavId>,
    pending_render: Option<(NavId, Instant)>,
    pending_add_target: Option<Project>,
    undo_stack: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    view_date: NaiveDate,
    last_sync_started: Instant,
    scheduled_sync_running: bool,
    transient_status: Option<(String, Instant)>,
    persistent_status: Option<String>,
}

impl App {
    pub fn new(
        store: Store,
        sync: SyncManager,
        keybindings: KeyBindings,
        config_path: std::path::PathBuf,
    ) -> Result<Self> {
        let nav_rows = store.nav_rows(false)?;
        let nav_cursor = nav_rows
            .iter()
            .position(|r| matches!(r, NavRow::Item { .. }))
            .unwrap_or(0);
        let nav_search = build_nav_search(&nav_rows, &keybindings);
        let has_token = sync.has_token();
        let task_view_preferences = store.task_view_preferences()?;
        let (status, sync_status, sync_issue) = if has_token {
            ("Ready".into(), SyncStatus::Pending, None)
        } else {
            let message = "API token missing: add it to API_TOKEN and restart".to_owned();
            (message.clone(), SyncStatus::Error, Some(message))
        };
        let persistent_status = Some(status.clone());
        let mut app = Self {
            store,
            sync,
            nav_rows,
            nav_cursor,
            tasks: Vec::new(),
            task_cursor: None,
            focus: Focus::Navigation,
            status,
            sync_status,
            sync_issue,
            overlay: None,
            prefix: None,
            quit: false,
            show_hidden_projects: false,
            group_tasks_by_label: task_view_preferences.group_by_label,
            sort_tasks_alphabetically: task_view_preferences.sort_alphabetically,
            show_completed_tasks: task_view_preferences.show_completed,
            keybindings,
            config_path,
            nav_search,
            remembered_tasks: HashMap::new(),
            rendered_nav: None,
            pending_render: None,
            pending_add_target: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            view_date: Local::now().date_naive(),
            last_sync_started: Instant::now(),
            scheduled_sync_running: false,
            transient_status: None,
            persistent_status,
        };
        app.force_tasks()?;
        if has_token && app.start_sync_run() {
            app.sync_status = SyncStatus::Syncing;
            app.sync_issue = None;
            app.set_persistent_status("Syncing…");
        }
        Ok(app)
    }

    pub fn selected_nav(&self) -> Option<&NavId> {
        match self.nav_rows.get(self.nav_cursor) {
            Some(NavRow::Item { id, .. }) => Some(id),
            _ => None,
        }
    }

    pub fn selected_nav_name(&self) -> &str {
        match self.nav_rows.get(self.nav_cursor) {
            Some(NavRow::Item { name, .. }) => name,
            _ => "Tasks",
        }
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.task_cursor.and_then(|i| self.tasks.get(i))
    }

    pub fn tick(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Some(event) = self.sync.next_event() {
            changed = true;
            self.handle_sync_event(event)?;
        }
        let today = Local::now().date_naive();
        if today != self.view_date {
            self.view_date = today;
            self.refresh_nav()?;
            self.force_tasks()?;
            changed = true;
        }
        if self.sync.has_token()
            && !self.sync.is_running()
            && self.last_sync_started.elapsed() >= SYNC_INTERVAL
        {
            // Scheduled refreshes are deliberately silent. Their results are
            // rendered when they arrive, while failures remain visible.
            self.start_scheduled_sync_run();
        }
        if let Some((id, deadline)) = self.pending_render.clone()
            && Instant::now() >= deadline
        {
            self.load_tasks(id)?;
            self.pending_render = None;
            changed = true;
        }
        let result = if let Some(Overlay::Editor(editor)) = self.overlay.as_mut() {
            changed |= editor.take_dirty();
            editor.poll()?
        } else {
            None
        };
        if let Some(result) = result {
            self.overlay = None;
            self.apply_editor_result(result)?;
            changed = true;
        }
        changed |= self.expire_or_track_status();
        Ok(changed)
    }

    pub fn editor_is_open(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Editor(_)))
    }

    pub fn editor_cursor_shape(&self) -> Option<EditorCursorShape> {
        match self.overlay.as_ref() {
            Some(Overlay::Editor(editor)) => Some(editor.cursor_shape()),
            _ => None,
        }
    }

    pub fn poll_timeout(&self) -> Duration {
        if self.editor_is_open() {
            return EDITOR_POLL_INTERVAL;
        }

        let now = Instant::now();
        let mut timeout = if self.sync.is_running() {
            SYNC_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        if let Some((_, deadline)) = self.pending_render.as_ref() {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some((_, deadline)) = self.transient_status.as_ref() {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if self.sync.has_token() && !self.sync.is_running() {
            timeout = timeout.min(SYNC_INTERVAL.saturating_sub(self.last_sync_started.elapsed()));
        }
        timeout
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(Overlay::Editor(editor)) = self.overlay.as_mut() {
            if editor.send_key(key)? {
                self.overlay = None;
                self.pending_add_target = None;
                self.status = "Edit cancelled".into();
            }
            return Ok(());
        }
        if self.overlay.is_some() {
            return self.handle_overlay(key);
        }
        if self.prefix.is_some() {
            return self.handle_prefix(key);
        }

        if let Some(project) = self.keybindings.special_add_project(key).map(str::to_owned) {
            self.open_special_add(&project)?;
            return Ok(());
        }

        let Some(action) = self.keybindings.main_action(key) else {
            return Ok(());
        };
        if let Some(space) = action.space() {
            self.move_to_space(space)?;
            return Ok(());
        }
        match action {
            Action::EditLabels => self.open_labels()?,
            Action::Help => self.overlay = Some(Overlay::Help),
            Action::Quit => self.quit = true,
            Action::Sync => self.start_sync(),
            Action::Undo => self.undo()?,
            Action::Redo => self.redo()?,
            Action::ToggleLabelGrouping => self.toggle_label_grouping()?,
            Action::ToggleAlphabeticalSort => self.toggle_alphabetical_sort()?,
            Action::ToggleCompletedTasks => self.toggle_completed_tasks()?,
            Action::ToggleHiddenProjects => self.toggle_hidden_projects()?,
            Action::ToggleProjectHidden => self.toggle_selected_navigation_hidden()?,
            Action::FocusNavigation => self.focus = Focus::Navigation,
            Action::FocusTasks => self.enter_tasks()?,
            Action::Confirm if self.focus == Focus::Navigation => self.enter_tasks()?,
            Action::Confirm => self.open_details(),
            Action::Down => self.move_down()?,
            Action::Up => self.move_up()?,
            Action::DownFive => self.move_down_by(5)?,
            Action::UpFive => self.move_up_by(5)?,
            Action::Last => self.move_last()?,
            Action::First => self.move_first()?,
            Action::Jump => self.prefix = Some(String::new()),
            Action::AddTask => self.open_add()?,
            Action::EditTask => self.open_edit()?,
            Action::EditDue => self.open_due()?,
            Action::CompleteTask => self.complete()?,
            Action::MoveTask => self.open_project_picker()?,
            Action::Cancel
            | Action::CloseDialog
            | Action::Space0
            | Action::Space1
            | Action::Space2
            | Action::Space3
            | Action::Space4
            | Action::Space5
            | Action::Space6
            | Action::Space7
            | Action::Space8
            | Action::Space9 => {}
        }
        Ok(())
    }

    fn handle_overlay(&mut self, key: KeyEvent) -> Result<()> {
        match self.overlay.as_mut() {
            Some(Overlay::Help | Overlay::Details(_))
                if self.keybindings.matches(Action::CloseDialog, key) =>
            {
                self.overlay = None;
            }
            Some(Overlay::Help | Overlay::Details(_)) => {}
            Some(Overlay::LabelPicker {
                task_id,
                query,
                cursor,
                mode,
                labels,
                task_labels,
            }) => {
                if self.keybindings.matches(Action::Cancel, key) {
                    if mode.cancel() {
                        self.overlay = None;
                        self.status = "Label change cancelled".into();
                    }
                } else if self.keybindings.matches(Action::Confirm, key) {
                    let matches = filtered_label_indices(labels, query);
                    if let Some(label) = matches
                        .get((*cursor).min(matches.len().saturating_sub(1)))
                        .and_then(|index| labels.get(*index))
                        .cloned()
                    {
                        let id = task_id.clone();
                        let (updated, added) = toggled_labels(task_labels, &label.name);
                        self.overlay = None;
                        let before = self
                            .store
                            .task(&id)?
                            .context("labelled task no longer exists")?;
                        self.store.update_labels(&id, &updated)?;
                        self.record_task_change(before, &id, "label change")?;
                        self.refresh_after_action(Some(&id))?;
                        self.status = format!(
                            "Label {} {} · pending sync",
                            label.name,
                            if added { "added" } else { "removed" }
                        );
                        self.mark_pending_and_sync();
                    } else {
                        self.status = "No label matches that prefix".into();
                    }
                } else if *mode == PickerMode::Selecting {
                    if self.keybindings.matches(Action::Up, key) {
                        *cursor = cursor.saturating_sub(1);
                    } else if self.keybindings.matches(Action::Down, key) {
                        let count = filtered_label_indices(labels, query).len();
                        if *cursor + 1 < count {
                            *cursor += 1;
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Backspace if !query.is_empty() => {
                            query.pop();
                            *cursor = 0;
                        }
                        KeyCode::Char(character)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.push(character);
                            *cursor = 0;
                        }
                        _ => {}
                    }
                }
            }
            Some(Overlay::ProjectPicker {
                task_id,
                query,
                cursor,
                mode,
                projects,
            }) => {
                if self.keybindings.matches(Action::Cancel, key) {
                    if mode.cancel() {
                        self.overlay = None;
                        self.status = "Task move cancelled".into();
                    }
                } else if self.keybindings.matches(Action::Confirm, key) {
                    let matches = filtered_project_indices(projects, query);
                    if let Some(project) = matches
                        .get((*cursor).min(matches.len().saturating_sub(1)))
                        .and_then(|index| projects.get(*index))
                        .cloned()
                    {
                        let id = task_id.clone();
                        self.overlay = None;
                        self.apply_task_move(&id, &project)?;
                    } else {
                        self.status = "No project matches that prefix".into();
                    }
                } else if *mode == PickerMode::Selecting {
                    if self.keybindings.matches(Action::Up, key) {
                        *cursor = cursor.saturating_sub(1);
                    } else if self.keybindings.matches(Action::Down, key) {
                        let count = filtered_project_indices(projects, query).len();
                        if *cursor + 1 < count {
                            *cursor += 1;
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Backspace if !query.is_empty() => {
                            query.pop();
                            *cursor = 0;
                        }
                        KeyCode::Char(character)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            query.push(character);
                            *cursor = 0;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_prefix(&mut self, key: KeyEvent) -> Result<()> {
        let prefix = self.prefix.as_mut().expect("prefix checked");
        match key.code {
            _ if self.keybindings.matches(Action::Cancel, key) => {
                self.prefix = None;
                self.status = "Jump cancelled".into();
            }
            _ if prefix.is_empty() && self.keybindings.follows_jump(Action::First, key) => {
                self.prefix = None;
                self.move_first()?;
            }
            KeyCode::Char(c) if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' => {
                prefix.extend(c.to_lowercase());
                let query = prefix.clone();
                if let Some(project) = self
                    .keybindings
                    .project_jump_target(&query)
                    .map(str::to_owned)
                {
                    let project_search = project.to_lowercase();
                    let project_match = unique_nav_match(
                        self.nav_search
                            .iter()
                            .filter(|entry| {
                                entry.is_project && entry.normalized_name == project_search
                            })
                            .map(|entry| entry.row_index),
                    );
                    self.prefix = None;
                    match project_match {
                        NavSearchMatch::Unique(index) => {
                            self.select_nav(index);
                            self.force_tasks()?;
                            self.focus = Focus::Tasks;
                            self.status = format!("Jumped to {}", self.selected_nav_name());
                        }
                        NavSearchMatch::None => {
                            self.status = format!("Configured project '#{project}' is not visible");
                        }
                        NavSearchMatch::Ambiguous => {
                            self.status = format!(
                                "Configured project '#{project}' is ambiguous; names must be unique"
                            );
                        }
                    }
                    return Ok(());
                }
                let name_match = unique_nav_match(
                    self.nav_search
                        .iter()
                        .filter(|entry| {
                            !entry.uses_custom_jump && entry.normalized_name.starts_with(&query)
                        })
                        .map(|entry| entry.row_index),
                );
                let configured_prefix_pending =
                    self.keybindings.has_project_jump_with_prefix(&query);
                if name_match == NavSearchMatch::None && !configured_prefix_pending {
                    self.prefix = None;
                    self.status = format!("No project or filter starts with ‘{query}’");
                } else if query.chars().count() >= 2
                    && !configured_prefix_pending
                    && let NavSearchMatch::Unique(index) = name_match
                {
                    self.prefix = None;
                    self.select_nav(index);
                    self.force_tasks()?;
                    self.focus = Focus::Tasks;
                    self.status = format!("Jumped to {}", self.selected_nav_name());
                } else {
                    self.status = format!("Jump: {query}");
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_down(&mut self) -> Result<()> {
        self.move_down_by(1)
    }

    fn move_down_by(&mut self, rows: usize) -> Result<()> {
        match self.focus {
            Focus::Navigation => {
                if let Some(index) = ((self.nav_cursor + 1)..self.nav_rows.len())
                    .find(|i| matches!(self.nav_rows[*i], NavRow::Item { .. }))
                {
                    let target = (index..self.nav_rows.len())
                        .filter(|i| matches!(self.nav_rows[*i], NavRow::Item { .. }))
                        .take(rows)
                        .last()
                        .unwrap_or(index);
                    self.select_nav(target);
                }
            }
            Focus::Tasks => {
                if let Some(current) = self.task_cursor
                    && current + 1 < self.tasks.len()
                {
                    self.set_task_cursor((current + rows).min(self.tasks.len() - 1));
                }
            }
        }
        Ok(())
    }

    fn move_up(&mut self) -> Result<()> {
        self.move_up_by(1)
    }

    fn move_up_by(&mut self, rows: usize) -> Result<()> {
        match self.focus {
            Focus::Navigation => {
                if let Some(index) = (0..self.nav_cursor)
                    .rev()
                    .find(|i| matches!(self.nav_rows[*i], NavRow::Item { .. }))
                {
                    let target = (0..=index)
                        .rev()
                        .filter(|i| matches!(self.nav_rows[*i], NavRow::Item { .. }))
                        .take(rows)
                        .last()
                        .unwrap_or(index);
                    self.select_nav(target);
                }
            }
            Focus::Tasks => {
                if let Some(current) = self.task_cursor
                    && current > 0
                {
                    self.set_task_cursor(current.saturating_sub(rows));
                }
            }
        }
        Ok(())
    }

    fn move_first(&mut self) -> Result<()> {
        match self.focus {
            Focus::Navigation => {
                if let Some(i) = self
                    .nav_rows
                    .iter()
                    .position(|r| matches!(r, NavRow::Item { .. }))
                {
                    self.select_nav(i);
                }
            }
            Focus::Tasks => {
                if !self.tasks.is_empty() {
                    self.set_task_cursor(0);
                }
            }
        }
        Ok(())
    }

    fn move_last(&mut self) -> Result<()> {
        match self.focus {
            Focus::Navigation => {
                if let Some(i) = self
                    .nav_rows
                    .iter()
                    .rposition(|r| matches!(r, NavRow::Item { .. }))
                {
                    self.select_nav(i);
                }
            }
            Focus::Tasks => {
                if !self.tasks.is_empty() {
                    self.set_task_cursor(self.tasks.len() - 1);
                }
            }
        }
        Ok(())
    }

    fn select_nav(&mut self, index: usize) {
        self.nav_cursor = index;
        if let Some(id) = self.selected_nav().cloned() {
            self.pending_render = Some((id, Instant::now() + NAV_DEBOUNCE));
        }
    }

    fn enter_tasks(&mut self) -> Result<()> {
        self.force_tasks()?;
        self.focus = Focus::Tasks;
        Ok(())
    }

    fn set_task_cursor(&mut self, index: usize) {
        self.task_cursor = Some(index);
        if let (Some(nav), Some(task)) = (self.selected_nav(), self.tasks.get(index)) {
            self.remembered_tasks.insert(nav.clone(), task.id.clone());
        }
    }

    fn force_tasks(&mut self) -> Result<()> {
        self.pending_render = None;
        if let Some(id) = self.selected_nav().cloned() {
            self.load_tasks(id)?;
        }
        Ok(())
    }

    fn load_tasks(&mut self, nav: NavId) -> Result<()> {
        let mut tasks = self.store.tasks_for_view(&nav, self.show_completed_tasks)?;
        order_tasks(
            &mut tasks,
            self.group_tasks_by_label,
            self.sort_tasks_alphabetically,
        );
        let remembered = self.remembered_tasks.get(&nav).cloned();
        let resolved = remembered
            .as_deref()
            .map(|id| self.store.resolve_task_id(id))
            .transpose()?
            .flatten();
        let cursor = remembered
            .as_ref()
            .and_then(|id| tasks.iter().position(|t| &t.id == id))
            .or_else(|| {
                resolved
                    .as_ref()
                    .and_then(|id| tasks.iter().position(|t| &t.id == id))
            })
            .or_else(|| (!tasks.is_empty()).then_some(0));
        self.tasks = tasks;
        self.task_cursor = cursor;
        self.rendered_nav = Some(nav.clone());
        if let Some(index) = cursor {
            self.remembered_tasks
                .insert(nav, self.tasks[index].id.clone());
        }
        Ok(())
    }

    fn refresh_nav(&mut self) -> Result<()> {
        let selected = self.selected_nav().cloned();
        self.nav_rows = self.store.nav_rows(self.show_hidden_projects)?;
        self.nav_search = build_nav_search(&self.nav_rows, &self.keybindings);
        self.nav_cursor = selected
            .as_ref()
            .and_then(|id| {
                self.nav_rows
                    .iter()
                    .position(|row| matches!(row, NavRow::Item { id: row_id, .. } if row_id == id))
            })
            .or_else(|| {
                self.nav_rows
                    .iter()
                    .position(|r| matches!(r, NavRow::Item { .. }))
            })
            .unwrap_or(0);
        Ok(())
    }

    fn refresh_after_action(&mut self, preferred: Option<&str>) -> Result<()> {
        let nav = self.selected_nav().cloned();
        self.refresh_nav()?;
        if let Some(nav) = nav {
            if let Some(id) = preferred {
                self.remembered_tasks.insert(nav.clone(), id.into());
            }
            self.load_tasks(nav)?;
        }
        Ok(())
    }

    fn open_details(&mut self) {
        if let Some(task) = self.selected_task().cloned() {
            self.overlay = Some(Overlay::Details(task));
        }
    }

    fn open_labels(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        if let Some(task) = self.selected_task().cloned() {
            let mut known = self.store.labels()?;
            for assigned in &task.labels {
                if !known
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(assigned))
                {
                    known.push(assigned.clone());
                }
            }
            if known.is_empty() {
                self.status = "No Todoist labels are available".into();
                return Ok(());
            }
            let labels = known
                .into_iter()
                .map(|name| LabelPickerItem {
                    search_name: name.to_lowercase(),
                    selected: task
                        .labels
                        .iter()
                        .any(|label| label.eq_ignore_ascii_case(&name)),
                    name,
                })
                .collect();
            self.overlay = Some(Overlay::LabelPicker {
                task_id: task.id,
                query: String::new(),
                cursor: 0,
                mode: PickerMode::Filtering,
                labels,
                task_labels: task.labels,
            });
        }
        Ok(())
    }

    fn open_add(&mut self) -> Result<()> {
        self.force_tasks()?;
        if self.add_target_project()?.is_none() {
            self.status = "No Inbox or selected project is available".into();
            return Ok(());
        }
        self.pending_add_target = None;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::Add,
            None,
            "",
            12,
            70,
        )?));
        Ok(())
    }

    fn open_special_add(&mut self, project_name: &str) -> Result<()> {
        let Some(project) = self.store.project_named(project_name)? else {
            self.status = format!("Special-add project '#{project_name}' is not available locally");
            return Ok(());
        };
        let editor = EmbeddedEditor::open(EditorPurpose::Add, None, "", 12, 70)?;
        self.pending_add_target = Some(project);
        self.overlay = Some(Overlay::Editor(editor));
        Ok(())
    }

    fn open_edit(&mut self) -> Result<()> {
        self.pending_add_target = None;
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        if let Some(task) = self.selected_task() {
            let initial = if task.description.is_empty() {
                task.content.clone()
            } else {
                format!("{}\n{}", task.content, task.description)
            };
            self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
                EditorPurpose::Edit,
                Some(task.id.clone()),
                &initial,
                16,
                76,
            )?));
        }
        Ok(())
    }

    fn open_due(&mut self) -> Result<()> {
        self.pending_add_target = None;
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        if let Some(task) = self.selected_task() {
            let initial = task
                .due
                .as_ref()
                .and_then(|d| d.string.clone())
                .unwrap_or_default();
            self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
                EditorPurpose::Due,
                Some(task.id.clone()),
                &initial,
                8,
                64,
            )?));
        }
        Ok(())
    }

    fn open_project_picker(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        let Some(task) = self.selected_task().cloned() else {
            return Ok(());
        };
        let projects = self
            .nav_rows
            .iter()
            .filter_map(|row| match row {
                NavRow::Item {
                    id: NavId::Project(id),
                    name,
                    ..
                } if id != &task.project_id => Some(ProjectPickerItem {
                    id: id.clone(),
                    name: name.clone(),
                    search_name: name.to_lowercase(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        if projects.is_empty() {
            self.status = "No other project is available".into();
            return Ok(());
        }
        self.overlay = Some(Overlay::ProjectPicker {
            task_id: task.id,
            query: String::new(),
            cursor: 0,
            mode: PickerMode::Filtering,
            projects,
        });
        Ok(())
    }

    fn apply_task_move(&mut self, task_id: &str, project: &ProjectPickerItem) -> Result<()> {
        let previous_index = self.task_cursor.unwrap_or(0);
        let nav = self.selected_nav().cloned();
        let before = self
            .store
            .task(task_id)?
            .context("moved task no longer exists")?;
        self.store.move_task(task_id, &project.id)?;
        self.record_task_change(before, task_id, "task move")?;
        self.refresh_nav()?;
        if let Some(nav) = nav {
            match &nav {
                NavId::Project(_) => {
                    self.remembered_tasks.remove(&nav);
                    self.load_tasks(nav)?;
                    if !self.tasks.is_empty() {
                        self.set_task_cursor(previous_index.min(self.tasks.len() - 1));
                    }
                }
                NavId::Filter(_) | NavId::Today | NavId::Upcoming => {
                    self.remembered_tasks
                        .insert(nav.clone(), task_id.to_owned());
                    self.load_tasks(nav)?;
                }
            }
        }
        self.focus = Focus::Tasks;
        self.status = format!("Task moved to {} · pending sync", project.name);
        self.mark_pending_and_sync();
        Ok(())
    }

    fn apply_editor_result(&mut self, result: EditorResult) -> Result<()> {
        if !result.accepted {
            self.pending_add_target = None;
            self.status = "Edit cancelled".into();
            return Ok(());
        }
        match result.purpose {
            EditorPurpose::Add => {
                let expression = result.text.trim();
                if expression.is_empty() {
                    self.pending_add_target = None;
                    self.status = "Empty task was not added".into();
                    return Ok(());
                }
                let special_project = self.pending_add_target.take();
                let special_project_name =
                    special_project.as_ref().map(|project| project.name.clone());
                let from_virtual_view = special_project.is_none()
                    && matches!(
                        self.selected_nav(),
                        Some(NavId::Filter(_) | NavId::Today | NavId::Upcoming)
                    );
                let project = if let Some(project) = special_project {
                    project
                } else {
                    self.add_target_project()?
                        .context("selected project and Inbox disappeared")?
                };
                let id = self.store.add_task(expression, &project)?;
                self.record_history(HistoryEntry::TaskAdd {
                    expression: expression.to_owned(),
                    project,
                    task_id: id.clone(),
                });
                self.refresh_after_action(Some(&id))?;
                self.focus = Focus::Tasks;
                self.status = if let Some(name) = special_project_name {
                    format!("Task added to #{name} · pending sync")
                } else if from_virtual_view {
                    "Task added to Inbox · pending sync".into()
                } else {
                    "Task added · pending sync".into()
                };
                self.mark_pending_and_sync();
            }
            EditorPurpose::Edit => {
                let id = result.task_id.context("missing edited task")?;
                let normalized = result.text.trim_end_matches(['\r', '\n']);
                let mut lines = normalized.lines();
                let title = lines.next().unwrap_or("").trim();
                if title.is_empty() {
                    self.status = "Empty title: edit not applied".into();
                    return Ok(());
                }
                let description = lines.collect::<Vec<_>>().join("\n");
                let before = self
                    .store
                    .task(&id)?
                    .context("edited task no longer exists")?;
                self.store.edit_task(&id, title, &description)?;
                self.record_task_change(before, &id, "task edit")?;
                self.refresh_after_action(Some(&id))?;
                self.status = "Task updated · pending sync".into();
                self.mark_pending_and_sync();
            }
            EditorPurpose::Due => {
                let id = result.task_id.context("missing task for due date")?;
                let before = self
                    .store
                    .task(&id)?
                    .context("dated task no longer exists")?;
                self.store.update_due(&id, result.text.trim())?;
                self.record_task_change(before, &id, "due-date change")?;
                self.refresh_after_action(Some(&id))?;
                self.status = "Due date updated · pending sync".into();
                self.mark_pending_and_sync();
            }
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        let Some(index) = self.task_cursor else {
            return Ok(());
        };
        let before = self.tasks[index].clone();
        let id = before.id.clone();
        let completed = !before.checked;
        let task_below = if completed {
            None
        } else {
            self.tasks.get(index + 1).map(|task| task.id.clone())
        };
        self.store.set_task_completed(&id, completed)?;
        self.record_task_change(
            before,
            &id,
            if completed {
                "task completion"
            } else {
                "task reopening"
            },
        )?;
        let nav = self.selected_nav().cloned();
        self.refresh_nav()?;
        if let Some(nav) = nav {
            if self.show_completed_tasks || !completed {
                self.remembered_tasks
                    .insert(nav.clone(), task_below.unwrap_or(id));
            } else {
                self.remembered_tasks.remove(&nav);
            }
            self.load_tasks(nav)?;
            if completed && !self.show_completed_tasks && !self.tasks.is_empty() {
                self.set_task_cursor(index.min(self.tasks.len() - 1));
            }
        }
        self.focus = Focus::Tasks;
        self.status = if completed {
            "Task completed · pending sync".into()
        } else {
            "Task reopened · pending sync".into()
        };
        self.mark_pending_and_sync();
        Ok(())
    }

    fn record_task_change(
        &mut self,
        before: Task,
        task_id: &str,
        action: &'static str,
    ) -> Result<()> {
        let after = self
            .store
            .task(task_id)?
            .context("changed task no longer exists")?;
        if before != after {
            self.record_history(HistoryEntry::TaskChange {
                before: Box::new(before),
                after: Box::new(after),
                action,
            });
        }
        Ok(())
    }

    fn record_history(&mut self, entry: HistoryEntry) {
        push_history(&mut self.undo_stack, entry);
        self.redo_stack.clear();
    }

    fn undo(&mut self) -> Result<()> {
        let Some(mut entry) = self.undo_stack.pop() else {
            self.status = "Nothing to undo".into();
            return Ok(());
        };
        let action = entry.action();
        let preferred = match self.apply_history_entry(&mut entry, true) {
            Ok(preferred) => preferred,
            Err(error) => {
                self.undo_stack.push(entry);
                return Err(error);
            }
        };
        push_history(&mut self.redo_stack, entry);
        self.refresh_after_action(preferred.as_deref())?;
        self.status = format!("Undid {action} · pending sync");
        self.mark_pending_and_sync();
        Ok(())
    }

    fn redo(&mut self) -> Result<()> {
        let Some(mut entry) = self.redo_stack.pop() else {
            self.status = "Nothing to redo".into();
            return Ok(());
        };
        let action = entry.action();
        let replaced_ids = match &entry {
            HistoryEntry::TaskAdd { task_id, .. } => {
                let mut ids = vec![task_id.clone()];
                if let Some(resolved) = self.store.resolve_task_id(task_id)? {
                    ids.push(resolved);
                }
                ids
            }
            HistoryEntry::TaskChange { .. } => Vec::new(),
        };
        let preferred = match self.apply_history_entry(&mut entry, false) {
            Ok(preferred) => preferred,
            Err(error) => {
                self.redo_stack.push(entry);
                return Err(error);
            }
        };
        if let HistoryEntry::TaskAdd { task_id, .. } = &entry {
            remap_history_task_ids(&mut self.undo_stack, &replaced_ids, task_id);
            remap_history_task_ids(&mut self.redo_stack, &replaced_ids, task_id);
        }
        push_history(&mut self.undo_stack, entry);
        self.refresh_after_action(preferred.as_deref())?;
        self.status = format!("Redid {action} · pending sync");
        self.mark_pending_and_sync();
        Ok(())
    }

    fn apply_history_entry(&self, entry: &mut HistoryEntry, undo: bool) -> Result<Option<String>> {
        match entry {
            HistoryEntry::TaskChange { before, after, .. } => self
                .store
                .apply_task_snapshot(if undo { before } else { after })
                .map(Some),
            HistoryEntry::TaskAdd { task_id, .. } if undo => {
                self.store.delete_task(task_id)?;
                Ok(None)
            }
            HistoryEntry::TaskAdd {
                expression,
                project,
                task_id,
            } => {
                let destination = if let Some(project) = self.store.project(&project.id)? {
                    Some(project)
                } else if project.is_inbox {
                    self.store
                        .projects()?
                        .into_iter()
                        .find(|item| item.is_inbox)
                } else {
                    None
                };
                let destination = destination.context("task destination no longer exists")?;
                *task_id = self.store.add_task(expression, &destination)?;
                *project = destination;
                Ok(Some(task_id.clone()))
            }
        }
    }

    fn add_target_project(&self) -> Result<Option<Project>> {
        let Some(selected) = self.selected_nav() else {
            return Ok(None);
        };
        match selected {
            NavId::Project(id) => self.store.project(id),
            NavId::Filter(_) | NavId::Today | NavId::Upcoming => Ok(self
                .store
                .projects()?
                .into_iter()
                .find(|project| project.is_inbox)),
        }
    }

    fn move_to_space(&mut self, space: u8) -> Result<()> {
        let Some(NavId::Project(id)) = self.selected_nav().cloned() else {
            self.status = "Built-in views and filters remain in space 0".into();
            return Ok(());
        };
        match self.store.toggle_space(&id, space) {
            Ok((active_space, removed)) => {
                self.refresh_nav()?;
                self.status = if removed {
                    format!("Space override removed; project returned to space {active_space}")
                } else {
                    format!("Project moved to space {space}")
                };
            }
            Err(error) => self.status = error.to_string(),
        }
        Ok(())
    }

    fn toggle_hidden_projects(&mut self) -> Result<()> {
        self.show_hidden_projects = !self.show_hidden_projects;
        self.refresh_nav()?;
        self.force_tasks()?;
        self.status = if self.show_hidden_projects {
            "Hidden items shown".into()
        } else {
            "Hidden items concealed".into()
        };
        Ok(())
    }

    fn toggle_label_grouping(&mut self) -> Result<()> {
        self.group_tasks_by_label = !self.group_tasks_by_label;
        self.persist_task_view_preferences()?;
        self.force_tasks()?;
        self.status = if self.group_tasks_by_label {
            "Tasks grouped by label".into()
        } else {
            "Task grouping disabled".into()
        };
        Ok(())
    }

    fn toggle_alphabetical_sort(&mut self) -> Result<()> {
        self.sort_tasks_alphabetically = !self.sort_tasks_alphabetically;
        self.persist_task_view_preferences()?;
        self.force_tasks()?;
        self.status = if self.sort_tasks_alphabetically {
            "Tasks sorted alphabetically".into()
        } else {
            "Tasks restored to Todoist order".into()
        };
        Ok(())
    }

    fn toggle_completed_tasks(&mut self) -> Result<()> {
        self.show_completed_tasks = !self.show_completed_tasks;
        self.persist_task_view_preferences()?;
        self.force_tasks()?;
        self.status = if self.show_completed_tasks {
            "Completed tasks shown".into()
        } else {
            "Completed tasks hidden".into()
        };
        Ok(())
    }

    fn persist_task_view_preferences(&self) -> Result<()> {
        self.store.set_task_view_preferences(TaskViewPreferences {
            group_by_label: self.group_tasks_by_label,
            sort_alphabetically: self.sort_tasks_alphabetically,
            show_completed: self.show_completed_tasks,
        })
    }

    fn toggle_selected_navigation_hidden(&mut self) -> Result<()> {
        let Some(nav) = self.selected_nav().cloned() else {
            return Ok(());
        };
        let name = self.selected_nav_name().to_owned();
        match self.store.toggle_navigation_hidden(&nav) {
            Ok(is_hidden) => {
                self.refresh_nav()?;
                self.force_tasks()?;
                self.status = if is_hidden {
                    format!("{name} added to hidden group")
                } else {
                    format!("{name} removed from hidden group")
                };
            }
            Err(error) => self.status = error.to_string(),
        }
        Ok(())
    }

    fn start_sync(&mut self) {
        self.scheduled_sync_running = false;
        if !self.sync.has_token() {
            let message = "API token missing: add it to API_TOKEN and restart".to_owned();
            self.sync_status = SyncStatus::Error;
            self.sync_issue = Some(message.clone());
            self.set_persistent_status(message);
        } else if self.sync.is_running() {
            self.sync_status = SyncStatus::Syncing;
            self.sync_issue = None;
            self.set_persistent_status("Synchronization is already in progress");
        } else if self.start_sync_run() {
            self.sync_status = SyncStatus::Syncing;
            self.sync_issue = None;
            self.set_persistent_status("Syncing…");
        } else {
            let message = "Unable to start synchronization".to_owned();
            self.sync_status = SyncStatus::Error;
            self.sync_issue = Some(message.clone());
            self.set_persistent_status(message);
        }
    }

    fn mark_pending_and_sync(&mut self) {
        self.scheduled_sync_running = false;
        self.sync_status.mark_local_change();
        if self.start_sync_run() {
            self.sync_status = SyncStatus::Syncing;
            self.sync_issue = None;
        }
    }

    fn start_sync_run(&mut self) -> bool {
        if self.sync.start() {
            self.last_sync_started = Instant::now();
            self.scheduled_sync_running = false;
            true
        } else {
            false
        }
    }

    fn start_scheduled_sync_run(&mut self) -> bool {
        if self.sync.start() {
            self.last_sync_started = Instant::now();
            self.scheduled_sync_running = true;
            true
        } else {
            false
        }
    }

    fn handle_sync_event(&mut self, event: SyncEvent) -> Result<()> {
        match event {
            SyncEvent::Started if self.scheduled_sync_running => {}
            SyncEvent::Started => {
                self.sync_status = SyncStatus::Syncing;
                self.sync_issue = None;
                self.set_persistent_status("Syncing…");
            }
            SyncEvent::Finished { pending } => {
                let was_scheduled = self.scheduled_sync_running;
                self.scheduled_sync_running = false;
                self.sync_issue = None;
                if pending == 0 {
                    let recovered = self.sync_status != SyncStatus::Synced;
                    self.sync_status = SyncStatus::Synced;
                    if !was_scheduled {
                        self.set_persistent_status("Synced");
                    } else if recovered {
                        self.set_persistent_status("Ready");
                    }
                } else if self.start_sync_run() {
                    self.sync_status = SyncStatus::Syncing;
                    self.set_persistent_status(format!(
                        "Syncing {pending} newly queued change(s)…"
                    ));
                } else {
                    self.sync_status = SyncStatus::Pending;
                    self.set_persistent_status(format!("Pending sync: {pending}"));
                }
                self.refresh_nav()?;
                self.force_tasks()?;
            }
            SyncEvent::Failed { kind, message } => {
                self.scheduled_sync_running = false;
                let display = match kind {
                    SyncFailureKind::Offline => format!("Cannot reach Todoist: {message}"),
                    SyncFailureKind::Problem => format!("Sync error: {message}"),
                };
                self.sync_status = match kind {
                    SyncFailureKind::Offline => SyncStatus::Offline,
                    SyncFailureKind::Problem => SyncStatus::Error,
                };
                self.sync_issue = Some(display.clone());
                self.set_persistent_status(display);
            }
        }
        Ok(())
    }

    fn expire_or_track_status(&mut self) -> bool {
        if self.persistent_status.as_deref() == Some(self.status.as_str()) {
            self.transient_status = None;
            return false;
        }
        let now = Instant::now();
        match self.transient_status.as_ref() {
            Some((message, deadline)) if message == &self.status && now >= *deadline => {
                self.status.clear();
                self.transient_status = None;
                true
            }
            Some((message, _)) if message == &self.status => false,
            _ if self.status.is_empty() => {
                self.transient_status = None;
                false
            }
            _ => {
                self.transient_status = Some((self.status.clone(), now + STATUS_MESSAGE_DURATION));
                false
            }
        }
    }

    fn set_persistent_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.persistent_status = Some(self.status.clone());
        self.transient_status = None;
    }
}

fn push_history(stack: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    if stack.len() == HISTORY_LIMIT {
        stack.remove(0);
    }
    stack.push(entry);
}

fn remap_history_task_ids(entries: &mut [HistoryEntry], old_ids: &[String], new_id: &str) {
    for entry in entries {
        match entry {
            HistoryEntry::TaskChange { before, after, .. } => {
                if old_ids.contains(&before.id) {
                    before.id = new_id.to_owned();
                }
                if old_ids.contains(&after.id) {
                    after.id = new_id.to_owned();
                }
            }
            HistoryEntry::TaskAdd { task_id, .. } if old_ids.contains(task_id) => {
                *task_id = new_id.to_owned();
            }
            HistoryEntry::TaskAdd { .. } => {}
        }
    }
}

fn build_nav_search(rows: &[NavRow], keybindings: &KeyBindings) -> Vec<NavSearchEntry> {
    rows.iter()
        .enumerate()
        .filter_map(|(row_index, row)| {
            let NavRow::Item { id, name, .. } = row else {
                return None;
            };
            let is_project = matches!(id, NavId::Project(_));
            Some(NavSearchEntry {
                row_index,
                normalized_name: name.to_lowercase(),
                is_project,
                uses_custom_jump: is_project && keybindings.project_uses_custom_jump(name),
            })
        })
        .collect()
}

fn unique_nav_match(mut indices: impl Iterator<Item = usize>) -> NavSearchMatch {
    let Some(first) = indices.next() else {
        return NavSearchMatch::None;
    };
    if indices.next().is_some() {
        NavSearchMatch::Ambiguous
    } else {
        NavSearchMatch::Unique(first)
    }
}

pub fn filtered_project_indices(projects: &[ProjectPickerItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| project.search_name.starts_with(&query).then_some(index))
        .collect()
}

pub fn filtered_label_indices(labels: &[LabelPickerItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    labels
        .iter()
        .enumerate()
        .filter_map(|(index, label)| label.search_name.starts_with(&query).then_some(index))
        .collect()
}

fn order_tasks(tasks: &mut [Task], group_by_label: bool, alphabetically: bool) {
    match (group_by_label, alphabetically) {
        (false, false) => {}
        (false, true) => {
            tasks.sort_by_cached_key(|task| (task.checked, task.content.to_lowercase()))
        }
        (true, false) => {
            tasks.sort_by_cached_key(|task| {
                (task.display_group().map(str::to_lowercase), task.checked)
            });
        }
        (true, true) => tasks.sort_by_cached_key(|task| {
            (
                task.display_group().map(str::to_lowercase),
                task.checked,
                task.content.to_lowercase(),
            )
        }),
    }
}

fn toggled_labels(labels: &[String], selected: &str) -> (Vec<String>, bool) {
    if labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(selected))
    {
        (
            labels
                .iter()
                .filter(|label| !label.eq_ignore_ascii_case(selected))
                .cloned()
                .collect(),
            false,
        )
    } else {
        let mut updated = labels.to_vec();
        updated.push(selected.to_owned());
        (updated, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, content: &str, labels: &[&str]) -> Task {
        Task {
            id: id.into(),
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

    fn task_ids(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|task| task.id.as_str()).collect()
    }

    #[test]
    fn configured_prefix_jumps_directly_to_its_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        store
            .merge_remote(
                &[
                    crate::model::RemoteProject {
                        id: "week-project".into(),
                        name: "Week".into(),
                        parent_id: None,
                        child_order: 1,
                        workspace_id: None,
                        is_inbox_project: false,
                        shared: false,
                        can_assign_tasks: false,
                        is_deleted: false,
                        is_archived: false,
                    },
                    crate::model::RemoteProject {
                        id: "weekend-project".into(),
                        name: "Weekend".into(),
                        parent_id: None,
                        child_order: 2,
                        workspace_id: None,
                        is_inbox_project: false,
                        shared: false,
                        can_assign_tasks: false,
                        is_deleted: false,
                        is_archived: false,
                    },
                ],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[project_jumps]\nww = '#week'\n").unwrap();
        let keybindings = KeyBindings::load(&config_path, true).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(store, sync, keybindings, config_path).unwrap();

        for key in ['g', 'w', 'w'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .unwrap();
        }

        assert!(matches!(
            app.selected_nav(),
            Some(NavId::Project(id)) if id == "week-project"
        ));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.status, "Jumped to Week");

        for key in ['g', 'w', 'e'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .unwrap();
        }
        assert!(matches!(
            app.selected_nav(),
            Some(NavId::Project(id)) if id == "weekend-project"
        ));
        assert_eq!(app.status, "Jumped to Weekend");
    }

    #[test]
    fn task_order_modes_are_independent_and_composable() {
        let original = vec![
            task("z", "Zoo", &[]),
            task("work-b", "beta", &["Work"]),
            task("home-a", "Alpha", &["Home"]),
            task("work-a", "alpha", &["Work"]),
        ];

        let mut normal = original.clone();
        order_tasks(&mut normal, false, false);
        assert_eq!(task_ids(&normal), ["z", "work-b", "home-a", "work-a"]);

        let mut alphabetical = original.clone();
        order_tasks(&mut alphabetical, false, true);
        assert_eq!(task_ids(&alphabetical), ["home-a", "work-a", "work-b", "z"]);

        let mut grouped = original.clone();
        order_tasks(&mut grouped, true, false);
        assert_eq!(task_ids(&grouped), ["z", "home-a", "work-b", "work-a"]);

        let mut grouped_alphabetical = original;
        order_tasks(&mut grouped_alphabetical, true, true);
        assert_eq!(
            task_ids(&grouped_alphabetical),
            ["z", "home-a", "work-a", "work-b"]
        );
    }

    #[test]
    fn five_row_navigation_moves_and_clamps_in_both_panes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.nav_rows = (0..8)
            .flat_map(|index| {
                let item = NavRow::Item {
                    id: NavId::Project(format!("project-{index}")),
                    name: format!("Project {index}"),
                    count: 0,
                    is_inbox: index == 0,
                };
                if index == 3 {
                    vec![NavRow::Separator, item]
                } else {
                    vec![item]
                }
            })
            .collect();
        app.nav_cursor = 0;

        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            app.selected_nav().cloned(),
            Some(NavId::Project("project-5".into()))
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            app.selected_nav().cloned(),
            Some(NavId::Project("project-0".into()))
        );

        app.focus = Focus::Tasks;
        app.tasks = (0..8)
            .map(|index| task(&index.to_string(), &format!("Task {index}"), &[]))
            .collect();
        app.task_cursor = Some(0);
        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.task_cursor, Some(5));
        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.task_cursor, Some(7));
        app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.task_cursor, Some(2));
        app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.task_cursor, Some(0));
    }

    #[test]
    fn task_order_modes_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("todoist-tui.db");
        let config_path = dir.path().join("config.toml");

        {
            let store = Store::open(&database_path).unwrap();
            let sync = SyncManager::new(store.clone(), None).unwrap();
            let mut app =
                App::new(store, sync, KeyBindings::defaults(), config_path.clone()).unwrap();
            assert!(app.group_tasks_by_label);
            assert!(!app.sort_tasks_alphabetically);
            assert!(!app.show_completed_tasks);

            app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
                .unwrap();
            app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
                .unwrap();
            app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT))
                .unwrap();
            assert!(!app.group_tasks_by_label);
            assert!(app.sort_tasks_alphabetically);
            assert!(app.show_completed_tasks);
        }

        let store = Store::open(database_path).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let app = App::new(store, sync, KeyBindings::defaults(), config_path).unwrap();
        assert!(!app.group_tasks_by_label);
        assert!(app.sort_tasks_alphabetically);
        assert!(app.show_completed_tasks);
    }

    #[test]
    fn reopening_completed_tasks_advances_to_the_task_below() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let first_id = store.add_task("First", &inbox).unwrap();
        let second_id = store.add_task("Second", &inbox).unwrap();
        let third_id = store.add_task("Third", &inbox).unwrap();
        store.set_task_completed(&first_id, true).unwrap();
        store.set_task_completed(&second_id, true).unwrap();
        store.set_task_completed(&third_id, true).unwrap();
        store
            .set_task_view_preferences(TaskViewPreferences {
                show_completed: true,
                ..TaskViewPreferences::default()
            })
            .unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.selected_task().map(|task| task.id.as_str()),
            Some(second_id.as_str())
        );
        assert!(app.selected_task().unwrap().checked);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.selected_task().map(|task| task.id.as_str()),
            Some(third_id.as_str())
        );
        assert!(app.selected_task().unwrap().checked);
        assert_eq!(app.status, "Task reopened · pending sync");
    }

    #[test]
    fn completion_can_be_undone_and_redone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let task_id = store.add_task("Undo me", &inbox).unwrap();
        store
            .set_task_view_preferences(TaskViewPreferences {
                show_completed: true,
                ..TaskViewPreferences::default()
            })
            .unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.selected_task().unwrap().checked);

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_task().unwrap().id, task_id);
        assert!(!app.selected_task().unwrap().checked);
        assert_eq!(app.status, "Undid task completion · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.selected_task().unwrap().checked);
        assert_eq!(app.status, "Redid task completion · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.status, "Nothing to redo");
    }

    #[test]
    fn added_task_can_be_undone_and_redone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Add,
            task_id: None,
            accepted: true,
            text: "Pack passport".into(),
        })
        .unwrap();
        assert_eq!(app.tasks.len(), 1);
        let original_id = app.tasks[0].id.clone();
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Edit,
            task_id: Some(original_id.clone()),
            accepted: true,
            text: "Pack passport and tickets".into(),
        })
        .unwrap();
        assert_eq!(app.tasks[0].content, "Pack passport and tickets");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tasks[0].content, "Pack passport");
        assert_eq!(app.status, "Undid task edit · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.tasks.is_empty());
        assert_eq!(app.status, "Undid task addition · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.tasks[0].content, "Pack passport");
        assert_ne!(app.tasks[0].id, original_id);
        assert_eq!(app.status, "Redid task addition · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.tasks[0].content, "Pack passport and tickets");
        assert_eq!(app.status, "Redid task edit · pending sync");
    }

    #[test]
    fn special_add_uses_its_configured_fallback_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        store
            .merge_remote(
                &[crate::model::RemoteProject {
                    id: "week-project".into(),
                    name: "Week".into(),
                    parent_id: None,
                    child_order: 1,
                    workspace_id: None,
                    is_inbox_project: false,
                    shared: false,
                    can_assign_tasks: false,
                    is_deleted: false,
                    is_archived: false,
                }],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let week = store.project_named("week").unwrap().unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.pending_add_target = Some(week.clone());

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Add,
            task_id: None,
            accepted: true,
            text: "Review priorities tomorrow".into(),
        })
        .unwrap();

        let tasks = store.tasks_for(&NavId::Project(week.id)).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].content, "Review priorities tomorrow");
        assert_eq!(app.status, "Task added to #Week · pending sync");
    }

    #[test]
    fn project_picker_filters_case_insensitive_prefixes() {
        let projects = vec![
            ProjectPickerItem {
                id: "1".into(),
                name: "Inbox".into(),
                search_name: "inbox".into(),
            },
            ProjectPickerItem {
                id: "2".into(),
                name: "Shopping".into(),
                search_name: "shopping".into(),
            },
            ProjectPickerItem {
                id: "3".into(),
                name: "Shared work".into(),
                search_name: "shared work".into(),
            },
        ];
        assert_eq!(filtered_project_indices(&projects, "sh"), [1, 2]);
        assert_eq!(filtered_project_indices(&projects, "SHOP"), [1]);
        assert!(filtered_project_indices(&projects, "missing").is_empty());
    }

    #[test]
    fn project_picker_requires_two_cancels_to_close() {
        let mut mode = PickerMode::Filtering;

        assert!(!mode.cancel());
        assert_eq!(mode, PickerMode::Selecting);
        assert!(mode.cancel());
    }

    #[test]
    fn label_picker_filters_and_toggles_case_insensitively() {
        let labels = vec![
            LabelPickerItem {
                name: "Errands".into(),
                selected: false,
                search_name: "errands".into(),
            },
            LabelPickerItem {
                name: "Home".into(),
                selected: true,
                search_name: "home".into(),
            },
        ];
        assert_eq!(filtered_label_indices(&labels, "ho"), [1]);

        let (removed, added) = toggled_labels(&["home".into(), "Work".into()], "Home");
        assert!(!added);
        assert_eq!(removed, ["Work"]);
        let (added_labels, added) = toggled_labels(&removed, "Errands");
        assert!(added);
        assert_eq!(added_labels, ["Work", "Errands"]);
    }

    #[test]
    fn local_changes_turn_synced_status_pending_but_preserve_offline_state() {
        let mut synced = SyncStatus::Synced;
        synced.mark_local_change();
        assert_eq!(synced, SyncStatus::Pending);

        let mut offline = SyncStatus::Offline;
        offline.mark_local_change();
        assert_eq!(offline, SyncStatus::Offline);

        let mut syncing = SyncStatus::Syncing;
        syncing.mark_local_change();
        assert_eq!(syncing, SyncStatus::Syncing);

        let mut error = SyncStatus::Error;
        error.mark_local_change();
        assert_eq!(error, SyncStatus::Error);
    }

    #[test]
    fn ordinary_status_messages_expire() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.status = "Jumped to Inbox".into();

        assert!(!app.expire_or_track_status());
        app.transient_status.as_mut().unwrap().1 = Instant::now();
        assert!(app.expire_or_track_status());
        assert!(app.status.is_empty());
    }

    #[test]
    fn event_loop_uses_idle_and_navigation_aware_poll_intervals() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        assert_eq!(app.poll_timeout(), IDLE_POLL_INTERVAL);
        app.select_nav(1);
        assert!(app.poll_timeout() <= NAV_DEBOUNCE);
    }

    #[test]
    fn sync_status_messages_do_not_expire() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.set_persistent_status("Syncing…");

        assert!(!app.expire_or_track_status());
        assert_eq!(app.status, "Syncing…");
        assert!(app.transient_status.is_none());
    }

    #[test]
    fn scheduled_sync_does_not_replace_the_current_status_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.status = "Task selected".into();
        app.sync_status = SyncStatus::Synced;
        app.sync_issue = None;
        app.scheduled_sync_running = true;

        app.handle_sync_event(SyncEvent::Started).unwrap();
        assert_eq!(app.status, "Task selected");
        assert_eq!(app.sync_status, SyncStatus::Synced);

        app.handle_sync_event(SyncEvent::Finished { pending: 0 })
            .unwrap();
        assert_eq!(app.status, "Task selected");
        assert_eq!(app.sync_status, SyncStatus::Synced);
        assert!(!app.scheduled_sync_running);
    }

    #[test]
    fn scheduled_sync_failures_remain_visible() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.status = "Task selected".into();
        app.sync_status = SyncStatus::Synced;
        app.sync_issue = None;
        app.scheduled_sync_running = true;

        app.handle_sync_event(SyncEvent::Failed {
            kind: SyncFailureKind::Offline,
            message: "timed out".into(),
        })
        .unwrap();

        assert_eq!(app.sync_status, SyncStatus::Offline);
        assert_eq!(app.status, "Cannot reach Todoist: timed out");
        assert_eq!(
            app.sync_issue.as_deref(),
            Some("Cannot reach Todoist: timed out")
        );
        assert!(!app.scheduled_sync_running);
    }

    #[test]
    fn quick_add_from_virtual_views_falls_back_to_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store
            .projects()
            .unwrap()
            .into_iter()
            .find(|project| project.is_inbox)
            .unwrap();
        store
            .toggle_navigation_hidden(&NavId::Project(inbox.id.clone()))
            .unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        assert_eq!(app.selected_nav(), Some(&NavId::Today));
        let target = app.add_target_project().unwrap().unwrap();
        assert_eq!(target.id, inbox.id);
        assert!(target.is_inbox);
    }
}

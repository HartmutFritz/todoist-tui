use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::{
    Matcher, Utf32String,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};

use crate::{
    clipboard,
    editor::{EditorCursorShape, EditorOptions, EditorPurpose, EditorResult, EmbeddedEditor},
    keymap::{Action, KeyBindings},
    model::{Filter, NavId, NavRow, Project, Task},
    store::{OUTBOX_PAGE_SIZE, OutboxPage, Store, TaskViewPreferences},
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
    Help {
        scroll: usize,
    },
    ErrorDetails {
        message: String,
        scroll: usize,
    },
    Details {
        task: Task,
        scroll: usize,
    },
    FilterDetails {
        filter: Filter,
        scroll: usize,
    },
    Outbox(OutboxView),
    LabelPicker {
        task_id: String,
        query: String,
        cursor: usize,
        mode: PickerMode,
        labels: Vec<LabelPickerItem>,
        task_labels: Vec<String>,
    },
    ProjectPicker {
        purpose: ProjectPickerPurpose,
        query: String,
        cursor: usize,
        mode: PickerMode,
        projects: Vec<ProjectPickerItem>,
        search: ProjectSearch,
    },
    TaskPicker {
        query: String,
        cursor: usize,
        mode: PickerMode,
        tasks: Vec<TaskPickerItem>,
    },
    FilterPicker {
        query: String,
        cursor: usize,
        mode: PickerMode,
        filters: Vec<FilterPickerItem>,
    },
    Editor(EmbeddedEditor),
    EditorRecovery {
        draft: Box<EditorDraft>,
        message: String,
    },
}

#[derive(Default)]
pub struct OutboxView {
    pub page: Option<OutboxPage>,
    pub cursor: usize,
    pub details: bool,
    pub scroll: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditorDraft {
    pub result: EditorResult,
    special_add_target: Option<Project>,
    fallback: Option<Project>,
    placement: AddPlacement,
}

#[derive(Debug, Clone)]
pub enum ProjectPickerPurpose {
    Jump,
    MoveTask { task_id: String },
}

#[derive(Debug, Clone)]
pub struct ProjectPickerItem {
    pub id: String,
    pub name: String,
}

/// One matcher allocation and one ranked result set per project-picker query.
/// Candidate names are an immutable local snapshot for the lifetime of a picker.
#[derive(Default)]
pub struct ProjectSearch {
    matcher: Matcher,
    names: Option<Vec<(Utf32String, String)>>,
    query: Option<String>,
    matches: Vec<usize>,
}

impl ProjectSearch {
    pub fn matches(&mut self, projects: &[ProjectPickerItem], query: &str) -> &[usize] {
        let query = query.trim();
        if self.query.as_deref() != Some(query) {
            self.matches = if query.is_empty() {
                (0..projects.len()).collect()
            } else {
                // Literal fuzzy terms: punctuation in actual project names is
                // searchable rather than being interpreted as shell operators.
                let pattern = Pattern::new(
                    query,
                    CaseMatching::Ignore,
                    Normalization::Smart,
                    AtomKind::Fuzzy,
                );
                let text_query = query
                    .trim_start_matches(|character: char| !character.is_alphanumeric())
                    .to_lowercase();
                let names = self.names.get_or_insert_with(|| {
                    projects
                        .iter()
                        .map(|project| {
                            (
                                Utf32String::from(project.name.as_str()),
                                project
                                    .name
                                    .trim_start_matches(|character: char| {
                                        !character.is_alphanumeric()
                                    })
                                    .trim_end()
                                    .to_lowercase(),
                            )
                        })
                        .collect()
                });
                let mut ranked = names
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (name, text_name))| {
                        pattern
                            .score(name.slice(..), &mut self.matcher)
                            .map(|score| {
                                let exact = !text_query.is_empty() && text_name == &text_query;
                                (index, exact, score)
                            })
                    })
                    .collect::<Vec<_>>();
                // Stable ties keep cached Todoist order; empty queries do too.
                ranked.sort_by_key(|(_, exact, score)| (!exact, std::cmp::Reverse(*score)));
                ranked.into_iter().map(|(index, _, _)| index).collect()
            };
            self.query = Some(query.into());
        }
        &self.matches
    }
}

#[derive(Debug, Clone)]
pub struct LabelPickerItem {
    pub name: String,
    pub selected: bool,
    search_name: String,
}

#[derive(Debug, Clone)]
pub struct TaskPickerItem {
    pub task: Task,
    pub project_name: String,
    search_name: String,
}

#[derive(Debug, Clone)]
pub struct FilterPickerItem {
    pub filter: Filter,
    search_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Filtering,
    Selecting,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AddPlacement {
    #[default]
    Natural,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
enum HistoryEntry {
    Change {
        before: Vec<Task>,
        after: Vec<Task>,
        action: &'static str,
    },
    Add {
        expression: String,
        description: String,
        project: Project,
        parent_id: Option<String>,
        task_id: String,
        placement: AddPlacement,
    },
    Delete {
        tasks: Vec<Task>,
    },
}

impl HistoryEntry {
    fn action(&self) -> &'static str {
        match self {
            Self::Change { action, .. } => action,
            Self::Add {
                parent_id: Some(_), ..
            } => "subtask addition",
            Self::Add { .. } => "task addition",
            Self::Delete { .. } => "task deletion",
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
    pub task_render_cache: crate::ui::TaskRenderCache,
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
    pending_add_placement: AddPlacement,
    undo_stack: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    view_date: NaiveDate,
    last_sync_started: Instant,
    scheduled_sync_running: bool,
    sync_follow_up_requested: bool,
    transient_status: Option<(String, Instant)>,
    persistent_status: Option<String>,
    editor_options: EditorOptions,
    editor_add_fallback: Option<Project>,
    editor_mutation_committed: bool,
    clipboard: clipboard::Clipboard,
    clipboard_success_message: &'static str,
    outbox_load: Option<std::sync::mpsc::Receiver<Result<OutboxPage>>>,
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
            task_render_cache: crate::ui::TaskRenderCache::default(),
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
            pending_add_placement: AddPlacement::Natural,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            view_date: Local::now().date_naive(),
            last_sync_started: Instant::now(),
            scheduled_sync_running: false,
            sync_follow_up_requested: false,
            transient_status: None,
            persistent_status,
            editor_options: EditorOptions::default(),
            editor_add_fallback: None,
            editor_mutation_committed: false,
            clipboard: clipboard::Clipboard::default(),
            clipboard_success_message: "Title copied",
            outbox_load: None,
        };
        app.force_tasks()?;
        if has_token && app.start_sync_run() {
            app.sync_status = SyncStatus::Syncing;
            app.sync_issue = None;
            app.set_persistent_status("Syncing…");
        }
        Ok(app)
    }

    pub fn set_editor_options(&mut self, options: EditorOptions) {
        self.editor_options = options;
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
        let outbox_result =
            self.outbox_load
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some(Err(anyhow::anyhow!("Outbox diagnostics worker stopped")))
                    }
                });
        if let Some(result) = outbox_result {
            self.outbox_load = None;
            if let Some(Overlay::Outbox(view)) = self.overlay.as_mut() {
                view.loading = false;
                match result {
                    Ok(page) => {
                        let selected_seq = view
                            .page
                            .as_ref()
                            .and_then(|page| page.entries.get(view.cursor))
                            .map(|entry| entry.seq);
                        view.cursor = selected_seq
                            .and_then(|seq| page.entries.iter().position(|entry| entry.seq == seq))
                            .unwrap_or(view.cursor.min(page.entries.len().saturating_sub(1)));
                        view.page = Some(page);
                        view.scroll = 0;
                        view.error = None;
                    }
                    Err(error) => view.error = Some(format!("{error:#}")),
                }
                changed = true;
            }
        }
        while let Some(event) = self.sync.next_event() {
            changed = true;
            self.handle_sync_event(event)?;
        }
        if let Some(result) = self.clipboard.poll() {
            self.status = match result {
                Ok(()) => self.clipboard_success_message.into(),
                Err(error) => error.to_string(),
            };
            changed = true;
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
            && self.sync.retry_after().is_zero()
            && (self.last_sync_started.elapsed() >= SYNC_INTERVAL || self.sync.retry_due())
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
        let mut timeout = if self.sync.is_running()
            || self.clipboard.is_running()
            || self.outbox_load.is_some()
        {
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
            let retry = self.sync.retry_after();
            let scheduled = SYNC_INTERVAL.saturating_sub(self.last_sync_started.elapsed());
            timeout = timeout.min(if retry.is_zero() {
                scheduled
            } else {
                retry.max(scheduled)
            });
        }
        timeout
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(Overlay::Editor(editor)) = self.overlay.as_mut() {
            if editor.send_key(key)? {
                self.overlay = None;
                self.pending_add_target = None;
                self.pending_add_placement = AddPlacement::Natural;
                self.editor_add_fallback = None;
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

        if self.focus == Focus::Navigation
            && self.keybindings.matches(Action::RenameProject, key)
            && matches!(self.selected_nav(), Some(NavId::Project(_)))
        {
            self.open_rename_project()?;
            return Ok(());
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
            Action::SearchProjects => self.open_project_search()?,
            Action::SearchTasks => self.open_task_search()?,
            Action::SearchFilters => self.open_filter_search()?,
            Action::AddFilter => self.open_add_filter()?,
            Action::AddProject => self.open_add_project()?,
            Action::MoveProjectDown => self.move_selected_item(true)?,
            Action::MoveProjectUp => self.move_selected_item(false)?,
            Action::CopyTitle => self.copy_selected_title()?,
            Action::Help => self.overlay = Some(Overlay::Help { scroll: 0 }),
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
            Action::AddTaskTop => self.open_add(AddPlacement::Top)?,
            Action::AddTaskBottom => self.open_add(AddPlacement::Bottom)?,
            Action::AddChildTask => self.open_add_child()?,
            Action::EditTask => self.open_edit()?,
            Action::EditDue => self.open_due()?,
            Action::RemoveDue => self.remove_selected_due()?,
            Action::ShowError => self.show_error_details(),
            Action::ShowOutbox => {
                self.overlay = Some(Overlay::Outbox(OutboxView::default()));
                self.load_outbox_page(0)?;
            }
            Action::CompleteTask => self.complete()?,
            Action::DeleteTask => self.delete_selected_task()?,
            Action::MoveTask => self.open_project_picker()?,
            Action::IndentTask => self.change_task_depth(true)?,
            Action::OutdentTask => self.change_task_depth(false)?,
            Action::Cancel
            | Action::RenameProject
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
        if matches!(self.overlay, Some(Overlay::Outbox(_))) {
            return self.handle_outbox(key);
        }
        if matches!(self.overlay, Some(Overlay::EditorRecovery { .. })) {
            if self.keybindings.matches(Action::Confirm, key)
                || self.keybindings.matches(Action::EditTask, key)
            {
                if let Some(Overlay::EditorRecovery { draft, message }) = self.overlay.take() {
                    self.reopen_editor_draft(*draft, message);
                }
            } else if self.keybindings.matches(Action::CloseDialog, key)
                || self.keybindings.matches(Action::Cancel, key)
            {
                self.overlay = None;
                self.pending_add_target = None;
                self.pending_add_placement = AddPlacement::Natural;
                self.editor_add_fallback = None;
                self.status = "Draft cancelled".into();
            }
            return Ok(());
        }
        match self.overlay.as_mut() {
            Some(Overlay::Help { .. }) if self.keybindings.matches(Action::CloseDialog, key) => {
                self.overlay = None;
            }
            Some(Overlay::Help { scroll }) if self.keybindings.matches(Action::Down, key) => {
                *scroll = scroll.saturating_add(1);
            }
            Some(Overlay::Help { scroll }) if self.keybindings.matches(Action::Up, key) => {
                *scroll = scroll.saturating_sub(1);
            }
            Some(Overlay::Help { .. }) => {}
            Some(Overlay::ErrorDetails { .. })
                if self.keybindings.matches(Action::CloseDialog, key) =>
            {
                self.overlay = None;
            }
            Some(
                Overlay::ErrorDetails { scroll, .. }
                | Overlay::Details { scroll, .. }
                | Overlay::FilterDetails { scroll, .. },
            ) if self.keybindings.matches(Action::Down, key) => {
                *scroll = scroll.saturating_add(1);
            }
            Some(
                Overlay::ErrorDetails { scroll, .. }
                | Overlay::Details { scroll, .. }
                | Overlay::FilterDetails { scroll, .. },
            ) if self.keybindings.matches(Action::Up, key) => {
                *scroll = scroll.saturating_sub(1);
            }
            Some(Overlay::ErrorDetails { .. }) => {}
            Some(Overlay::Details { task, .. })
                if self.keybindings.matches(Action::EditTask, key) =>
            {
                let task_id = task.id.clone();
                self.open_task_editor(&task_id)?;
            }
            Some(Overlay::Details { task, .. })
                if self.keybindings.matches(Action::EditDue, key) =>
            {
                let task_id = task.id.clone();
                self.open_due_editor(&task_id)?;
            }
            Some(Overlay::Details { task, .. })
                if self.keybindings.matches(Action::RemoveDue, key) =>
            {
                let task_id = task.id.clone();
                self.overlay = None;
                self.remove_due(&task_id)?;
            }
            Some(Overlay::Details { task, .. })
                if self.keybindings.matches(Action::DeleteTask, key) =>
            {
                let task_id = task.id.clone();
                self.overlay = None;
                self.delete_task(&task_id)?;
            }
            Some(Overlay::FilterDetails { filter, .. })
                if self.keybindings.matches(Action::EditTask, key) =>
            {
                let filter_id = filter.id.clone();
                self.open_filter_editor(&filter_id)?;
            }
            Some(Overlay::Details { .. } | Overlay::FilterDetails { .. })
                if self.keybindings.matches(Action::CloseDialog, key) =>
            {
                self.overlay = None;
            }
            Some(Overlay::Details { .. } | Overlay::FilterDetails { .. }) => {}
            Some(Overlay::Outbox(_)) => unreachable!("outbox handled above"),
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
                purpose,
                query,
                cursor,
                mode,
                projects,
                search,
            }) => {
                if self.keybindings.matches(Action::Cancel, key) {
                    if mode.cancel() {
                        let message = match purpose {
                            ProjectPickerPurpose::Jump => "Project search cancelled",
                            ProjectPickerPurpose::MoveTask { .. } => "Task move cancelled",
                        };
                        self.overlay = None;
                        self.status = message.into();
                    }
                } else if self.keybindings.matches(Action::Confirm, key) {
                    let matches = search.matches(projects, query);
                    if let Some(project) = matches
                        .get((*cursor).min(matches.len().saturating_sub(1)))
                        .and_then(|index| projects.get(*index))
                        .cloned()
                    {
                        let purpose = purpose.clone();
                        self.overlay = None;
                        match purpose {
                            ProjectPickerPurpose::Jump => self.jump_to_project(&project)?,
                            ProjectPickerPurpose::MoveTask { task_id } => {
                                self.apply_task_move(&task_id, &project)?;
                            }
                        }
                    } else {
                        self.status = "No project matches that search".into();
                    }
                } else if *mode == PickerMode::Selecting {
                    if self.keybindings.matches(Action::Up, key) {
                        *cursor = cursor.saturating_sub(1);
                    } else if self.keybindings.matches(Action::Down, key) {
                        let count = search.matches(projects, query).len();
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
            Some(Overlay::TaskPicker {
                query,
                cursor,
                mode,
                tasks,
            }) => {
                if self.keybindings.matches(Action::Cancel, key) {
                    if mode.cancel() {
                        self.overlay = None;
                        self.status = "Task search cancelled".into();
                    }
                } else if self.keybindings.matches(Action::Confirm, key) {
                    let matches = filtered_task_indices(tasks, query);
                    if let Some(task) = matches
                        .get((*cursor).min(matches.len().saturating_sub(1)))
                        .and_then(|index| tasks.get(*index))
                        .map(|item| item.task.clone())
                    {
                        self.overlay = Some(Overlay::Details { task, scroll: 0 });
                    } else {
                        self.status = "No task matches that prefix".into();
                    }
                } else if *mode == PickerMode::Selecting {
                    if self.keybindings.matches(Action::Up, key) {
                        *cursor = cursor.saturating_sub(1);
                    } else if self.keybindings.matches(Action::Down, key) {
                        let count = filtered_task_indices(tasks, query).len();
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
            Some(Overlay::FilterPicker {
                query,
                cursor,
                mode,
                filters,
            }) => {
                if self.keybindings.matches(Action::Cancel, key) {
                    if mode.cancel() {
                        self.overlay = None;
                        self.status = "Filter search cancelled".into();
                    }
                } else if self.keybindings.matches(Action::Confirm, key) {
                    let matches = filtered_filter_indices(filters, query);
                    if let Some(filter) = matches
                        .get((*cursor).min(matches.len().saturating_sub(1)))
                        .and_then(|index| filters.get(*index))
                        .map(|item| item.filter.clone())
                    {
                        self.overlay = Some(Overlay::FilterDetails { filter, scroll: 0 });
                    } else {
                        self.status = "No filter matches that prefix".into();
                    }
                } else if *mode == PickerMode::Selecting {
                    if self.keybindings.matches(Action::Up, key) {
                        *cursor = cursor.saturating_sub(1);
                    } else if self.keybindings.matches(Action::Down, key) {
                        let count = filtered_filter_indices(filters, query).len();
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

    fn load_outbox_page(&mut self, offset: usize) -> Result<()> {
        let store = self.store.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("todoist-tui-outbox".into())
            .spawn(move || {
                let _ = sender.send(store.outbox_diagnostics(offset));
            })
            .context("start local outbox diagnostics")?;
        self.outbox_load = Some(receiver);
        if let Some(Overlay::Outbox(view)) = self.overlay.as_mut() {
            view.loading = true;
            view.error = None;
        }
        Ok(())
    }

    fn handle_outbox(&mut self, key: KeyEvent) -> Result<()> {
        let Some(Overlay::Outbox(view)) = self.overlay.as_mut() else {
            return Ok(());
        };
        let mut load = None;
        if view.details {
            if self.keybindings.matches(Action::CloseDialog, key)
                || self.keybindings.matches(Action::Cancel, key)
            {
                view.details = false;
                view.scroll = 0;
            } else if self.keybindings.matches(Action::Down, key) {
                view.scroll = view.scroll.saturating_add(1);
            } else if self.keybindings.matches(Action::Up, key) {
                view.scroll = view.scroll.saturating_sub(1);
            }
        } else if self.keybindings.matches(Action::Confirm, key) {
            if !view.loading
                && view.error.is_none()
                && view
                    .page
                    .as_ref()
                    .is_some_and(|page| !page.entries.is_empty())
            {
                view.details = true;
                view.scroll = 0;
            }
        } else if self.keybindings.matches(Action::CloseDialog, key)
            || self.keybindings.matches(Action::Cancel, key)
        {
            self.overlay = None;
        } else if !view.loading {
            let offset = view.page.as_ref().map_or(0, |page| page.offset);
            if self.keybindings.matches(Action::Down, key) {
                let len = view.page.as_ref().map_or(0, |page| page.entries.len());
                view.cursor = (view.cursor + 1).min(len.saturating_sub(1));
            } else if self.keybindings.matches(Action::Up, key) {
                view.cursor = view.cursor.saturating_sub(1);
            } else if self.keybindings.matches(Action::FocusTasks, key)
                && view
                    .page
                    .as_ref()
                    .is_some_and(|page| page.offset + page.entries.len() < page.total)
            {
                view.cursor = 0;
                load = Some(offset + OUTBOX_PAGE_SIZE);
            } else if self.keybindings.matches(Action::FocusNavigation, key) && offset > 0 {
                view.cursor = 0;
                load = Some(offset.saturating_sub(OUTBOX_PAGE_SIZE));
            } else if self.keybindings.matches(Action::Sync, key) {
                load = Some(offset);
            }
        }
        if let Some(offset) = load {
            self.load_outbox_page(offset)?;
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
            if self.rendered_nav.as_ref() != Some(&id) {
                // A failed load must not leave another view's tasks actionable.
                self.tasks.clear();
                self.task_render_cache.invalidate();
                self.task_cursor = None;
                self.rendered_nav = None;
            }
            self.load_tasks(id)?;
        } else {
            self.tasks.clear();
            self.task_render_cache.invalidate();
            self.task_cursor = None;
            self.rendered_nav = None;
        }
        Ok(())
    }

    fn load_tasks(&mut self, nav: NavId) -> Result<()> {
        let mut tasks = self.store.tasks_for_view(&nav, self.show_completed_tasks)?;
        let group_by_date = nav == NavId::Upcoming;
        order_tasks(
            &mut tasks,
            self.group_tasks_by_label && !group_by_date,
            self.sort_tasks_alphabetically,
            group_by_date,
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
        self.task_render_cache.invalidate();
        self.task_cursor = cursor;
        self.rendered_nav = Some(nav.clone());
        if let Some(index) = cursor {
            self.remembered_tasks
                .insert(nav, self.tasks[index].id.clone());
        }
        Ok(())
    }

    fn refresh_nav(&mut self) -> Result<()> {
        let selected = match self.selected_nav().cloned() {
            Some(NavId::Filter(id)) => self
                .store
                .filter(&id)?
                .map(|filter| NavId::Filter(filter.id)),
            Some(NavId::Project(id)) => self
                .store
                .project(&id)?
                .map(|project| NavId::Project(project.id)),
            selected => selected,
        };
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
            self.overlay = Some(Overlay::Details { task, scroll: 0 });
        }
    }

    fn copy_selected_title(&mut self) -> Result<()> {
        self.copy_selected_title_with(|clipboard, text| clipboard.start(text))
    }

    fn copy_selected_title_with(
        &mut self,
        copy: impl FnOnce(&mut clipboard::Clipboard, String) -> Result<bool>,
    ) -> Result<()> {
        let (title, success_message) = match self.focus {
            Focus::Navigation => {
                let Some(NavRow::Item { name, .. }) = self.nav_rows.get(self.nav_cursor) else {
                    self.status = "Select a project or filter before copying".into();
                    return Ok(());
                };
                (name.clone(), "Project or filter title copied")
            }
            Focus::Tasks => {
                self.force_tasks()?;
                let Some(task) = self.selected_task() else {
                    self.status = "Select a task before copying".into();
                    return Ok(());
                };
                (task.content.clone(), "Task title copied")
            }
        };

        self.status = match copy(&mut self.clipboard, title) {
            Ok(true) => {
                self.clipboard_success_message = success_message;
                "Copying title…".into()
            }
            Ok(false) => "Clipboard copy is already in progress".into(),
            Err(error) => error.to_string(),
        };
        Ok(())
    }

    fn open_task_search(&mut self) -> Result<()> {
        let tasks = self
            .store
            .tasks_for_search()?
            .into_iter()
            .map(|(task, project_name)| TaskPickerItem {
                search_name: task.content.to_lowercase(),
                task,
                project_name,
            })
            .collect::<Vec<_>>();
        if tasks.is_empty() {
            self.status = "No tasks are available to search".into();
            return Ok(());
        }
        self.overlay = Some(Overlay::TaskPicker {
            query: String::new(),
            cursor: 0,
            mode: PickerMode::Filtering,
            tasks,
        });
        Ok(())
    }

    fn open_project_search(&mut self) -> Result<()> {
        let projects = self
            .store
            .projects()?
            .into_iter()
            .map(|project| ProjectPickerItem {
                id: project.id,
                name: project.name,
            })
            .collect::<Vec<_>>();
        if projects.is_empty() {
            self.status = "No projects are available to search".into();
            return Ok(());
        }
        self.overlay = Some(Overlay::ProjectPicker {
            purpose: ProjectPickerPurpose::Jump,
            query: String::new(),
            cursor: 0,
            mode: PickerMode::Filtering,
            projects,
            search: ProjectSearch::default(),
        });
        Ok(())
    }

    fn open_filter_search(&mut self) -> Result<()> {
        let filters = self
            .store
            .filters()?
            .into_iter()
            .map(|filter| FilterPickerItem {
                search_name: filter.name.to_lowercase(),
                filter,
            })
            .collect::<Vec<_>>();
        if filters.is_empty() {
            self.status = "No filters are available to search".into();
            return Ok(());
        }
        self.overlay = Some(Overlay::FilterPicker {
            query: String::new(),
            cursor: 0,
            mode: PickerMode::Filtering,
            filters,
        });
        Ok(())
    }

    fn open_add_filter(&mut self) -> Result<()> {
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::AddFilter,
            None,
            "\n\nfavorite = true",
            10,
            76,
            self.editor_options,
        )?));
        Ok(())
    }

    fn open_add_project(&mut self) -> Result<()> {
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::AddProject,
            None,
            "",
            8,
            70,
            self.editor_options,
        )?));
        Ok(())
    }

    fn open_rename_project(&mut self) -> Result<()> {
        let Some(NavId::Project(project_id)) = self.selected_nav().cloned() else {
            self.status = "Select a project before renaming it".into();
            return Ok(());
        };
        let Some(project) = self.store.project(&project_id)? else {
            self.status = "Project is no longer available".into();
            return Ok(());
        };
        if project.is_inbox {
            self.status = "Inbox cannot be renamed".into();
            return Ok(());
        }
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::RenameProject,
            Some(project.id),
            &project.name,
            8,
            70,
            self.editor_options,
        )?));
        Ok(())
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

    fn open_add(&mut self, requested: AddPlacement) -> Result<()> {
        self.force_tasks()?;
        let Some(project) = self.add_target_project()? else {
            self.status = "No Inbox or selected project is available".into();
            return Ok(());
        };
        self.editor_add_fallback = Some(project);
        self.pending_add_target = None;
        self.pending_add_placement = self.effective_add_placement(requested);
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::Add,
            None,
            "\n\n",
            12,
            70,
            self.editor_options,
        )?));
        Ok(())
    }

    fn effective_add_placement(&self, requested: AddPlacement) -> AddPlacement {
        // Record explicit top/bottom placement in real projects even while
        // alphabetical sorting controls the displayed order, so the durable
        // Todoist order places the task there once sorting is disabled.
        if matches!(self.selected_nav(), Some(NavId::Project(_))) {
            requested
        } else {
            AddPlacement::Natural
        }
    }

    fn open_special_add(&mut self, project_name: &str) -> Result<()> {
        let Some(project) = self.store.project_named(project_name)? else {
            self.status = format!("Special-add project '#{project_name}' is not available locally");
            return Ok(());
        };
        let editor = EmbeddedEditor::open(
            EditorPurpose::Add,
            None,
            "\n\n",
            12,
            70,
            self.editor_options,
        )?;
        self.pending_add_target = Some(project);
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(editor));
        Ok(())
    }

    fn open_add_child(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            self.status = "Select a task before adding a subtask".into();
            return Ok(());
        }
        self.force_tasks()?;
        let Some(parent) = self.selected_task() else {
            self.status = "Select a task before adding a subtask".into();
            return Ok(());
        };
        if parent.checked {
            self.status = "Cannot add a subtask to a completed task".into();
            return Ok(());
        }
        let parent_id = parent.id.clone();
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::AddChild,
            Some(parent_id),
            "\n\n",
            12,
            70,
            self.editor_options,
        )?));
        Ok(())
    }

    fn open_edit(&mut self) -> Result<()> {
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        if let Some(task_id) = self.selected_task().map(|task| task.id.clone()) {
            self.open_task_editor(&task_id)?;
        }
        Ok(())
    }

    fn open_task_editor(&mut self, task_id: &str) -> Result<()> {
        let Some(task) = self.store.task(task_id)? else {
            self.overlay = None;
            self.status = "Task is no longer available".into();
            return Ok(());
        };
        let initial = if task.description.is_empty() {
            // Keep a real empty buffer line available for the description.
            // A single trailing newline is only the file's final EOL in
            // Neovim, so two are needed to create the editable blank line.
            format!("{}\n\n", task.content)
        } else {
            format!("{}\n{}", task.content, task.description)
        };
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::Edit,
            Some(task.id),
            &initial,
            16,
            76,
            self.editor_options,
        )?));
        Ok(())
    }

    fn open_filter_editor(&mut self, filter_id: &str) -> Result<()> {
        let Some(filter) = self.store.filter(filter_id)? else {
            self.overlay = None;
            self.status = "Filter is no longer available".into();
            return Ok(());
        };
        if filter.is_frozen {
            self.overlay = Some(Overlay::FilterDetails { filter, scroll: 0 });
            self.status = "This filter is read-only".into();
            return Ok(());
        }
        let initial = format!(
            "{}\n{}\nfavorite = {}",
            filter.name, filter.query, filter.is_favorite
        );
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::EditFilter,
            Some(filter.id),
            &initial,
            10,
            76,
            self.editor_options,
        )?));
        Ok(())
    }

    fn open_due(&mut self) -> Result<()> {
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        if let Some(task_id) = self.selected_task().map(|task| task.id.clone()) {
            self.open_due_editor(&task_id)?;
        }
        Ok(())
    }

    fn open_due_editor(&mut self, task_id: &str) -> Result<()> {
        let Some(task) = self.store.task(task_id)? else {
            self.overlay = None;
            self.status = "Task is no longer available".into();
            return Ok(());
        };
        let initial = task
            .due
            .as_ref()
            .and_then(|due| due.string.clone())
            .unwrap_or_default();
        self.pending_add_target = None;
        self.pending_add_placement = AddPlacement::Natural;
        self.overlay = Some(Overlay::Editor(EmbeddedEditor::open(
            EditorPurpose::Due,
            Some(task.id),
            &initial,
            8,
            64,
            self.editor_options,
        )?));
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
            .store
            .projects()?
            .into_iter()
            .filter(|project| project.id != task.project_id)
            .map(|project| ProjectPickerItem {
                id: project.id,
                name: project.name,
            })
            .collect::<Vec<_>>();
        if projects.is_empty() {
            self.status = "No other project is available".into();
            return Ok(());
        }
        self.overlay = Some(Overlay::ProjectPicker {
            purpose: ProjectPickerPurpose::MoveTask { task_id: task.id },
            query: String::new(),
            cursor: 0,
            mode: PickerMode::Filtering,
            projects,
            search: ProjectSearch::default(),
        });
        Ok(())
    }

    fn jump_to_project(&mut self, project: &ProjectPickerItem) -> Result<()> {
        let target = NavId::Project(project.id.clone());
        self.refresh_nav()?;
        let mut index = self
            .nav_rows
            .iter()
            .position(|row| matches!(row, NavRow::Item { id, .. } if id == &target));
        if index.is_none() && !self.show_hidden_projects {
            self.show_hidden_projects = true;
            self.refresh_nav()?;
            index = self
                .nav_rows
                .iter()
                .position(|row| matches!(row, NavRow::Item { id, .. } if id == &target));
        }
        let Some(index) = index else {
            self.status = format!("Project ‘{}’ is no longer available", project.name);
            return Ok(());
        };
        self.select_nav(index);
        self.force_tasks()?;
        self.focus = Focus::Tasks;
        self.status = format!("Jumped to {}", project.name);
        Ok(())
    }

    fn apply_task_move(&mut self, task_id: &str, project: &ProjectPickerItem) -> Result<()> {
        if let Some(message) = self.store.task_move_restriction(task_id, &project.id)? {
            self.status = message.into();
            return Ok(());
        }
        let previous_index = self.task_cursor.unwrap_or(0);
        let nav = self.selected_nav().cloned();
        let before = self
            .store
            .task(task_id)?
            .context("moved task no longer exists")?;
        let tree_before = self
            .store
            .tasks_for_view(&NavId::Project(before.project_id.clone()), true)?;
        self.store.move_task(task_id, &project.id)?;
        self.record_tree_change(tree_before, "task move", task_id)?;
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

    fn change_task_depth(&mut self, indent: bool) -> Result<()> {
        if self.focus != Focus::Tasks {
            self.status = "Select a task before changing its hierarchy".into();
            return Ok(());
        }
        self.force_tasks()?;
        let Some(index) = self.task_cursor else {
            self.status = "Select a task before changing its hierarchy".into();
            return Ok(());
        };
        let task = self.tasks[index].clone();
        if task.checked {
            self.status = "Cannot change the hierarchy of a completed task".into();
            return Ok(());
        }

        let new_parent_id = if indent {
            let candidate = self.tasks[..index].iter().rev().find(|candidate| {
                !candidate.checked
                    && candidate.project_id == task.project_id
                    && candidate.parent_id == task.parent_id
                    && candidate.section_id == task.section_id
            });
            let Some(candidate) = candidate else {
                self.status = "Cannot indent: no preceding sibling can become the parent".into();
                return Ok(());
            };
            Some(candidate.id.clone())
        } else {
            let Some(parent_id) = task.parent_id.as_deref() else {
                self.status = "Cannot outdent: task is already at the top level".into();
                return Ok(());
            };
            let Some(parent) = self.store.task(parent_id)? else {
                self.status = "Cannot outdent: parent task is no longer available".into();
                return Ok(());
            };
            parent.parent_id
        };

        let task_id = task.id.clone();
        self.store
            .set_task_parent(&task_id, new_parent_id.as_deref())?;
        self.record_task_change(
            task,
            &task_id,
            if indent {
                "task indent"
            } else {
                "task outdent"
            },
        )?;
        self.refresh_after_action(Some(&task_id))?;
        self.focus = Focus::Tasks;
        self.status = if indent {
            "Task indented · pending sync".into()
        } else {
            "Task outdented · pending sync".into()
        };
        self.mark_pending_and_sync();
        Ok(())
    }

    fn apply_editor_result(&mut self, result: EditorResult) -> Result<()> {
        if !result.accepted {
            self.editor_add_fallback = None;
            return self.apply_editor_result_inner(result);
        }
        let draft = EditorDraft {
            result: result.clone(),
            special_add_target: self.pending_add_target.clone(),
            fallback: self.editor_add_fallback.clone(),
            placement: self.pending_add_placement,
        };
        let validation = match result.purpose {
            EditorPurpose::Add | EditorPurpose::AddChild | EditorPurpose::Edit => {
                if split_task_editor_text(&result.text).0.is_empty() {
                    Err("Task title cannot be empty".into())
                } else {
                    Ok(())
                }
            }
            EditorPurpose::AddFilter | EditorPurpose::EditFilter => {
                parse_filter_editor(&result.text).map(|_| ())
            }
            EditorPurpose::AddProject | EditorPurpose::RenameProject => {
                parse_project_editor(&result.text).map(|_| ())
            }
            EditorPurpose::Due => Ok(()),
        };
        if let Err(message) = validation {
            self.reopen_editor_draft(draft, message);
            return Ok(());
        }
        self.editor_mutation_committed = false;
        match self.apply_editor_result_inner(result) {
            Ok(()) => {
                self.editor_add_fallback = None;
                Ok(())
            }
            Err(error) if !self.editor_mutation_committed => {
                self.reopen_editor_draft(draft, format!("Save failed: {error:#}"));
                Ok(())
            }
            Err(error) => {
                // The write succeeded; a failed history/view refresh must never
                // reopen an add document and duplicate an already-created task.
                self.editor_add_fallback = None;
                self.mark_pending_and_sync();
                Err(error)
            }
        }
    }

    fn reopen_editor_draft(&mut self, draft: EditorDraft, message: String) {
        self.pending_add_target = draft.special_add_target.clone();
        self.pending_add_placement = draft.placement;
        self.editor_add_fallback = draft.fallback.clone();
        self.status = message.clone();
        match EmbeddedEditor::open(
            draft.result.purpose,
            draft.result.task_id.clone(),
            &draft.result.text,
            14,
            76,
            self.editor_options,
        ) {
            Ok(mut editor) => {
                editor.set_notice(message);
                self.overlay = Some(Overlay::Editor(editor));
            }
            Err(error) => {
                self.overlay = Some(Overlay::EditorRecovery {
                    draft: Box::new(draft),
                    message: format!("{message}\nCould not reopen Neovim: {error:#}"),
                });
            }
        }
    }

    fn apply_editor_result_inner(&mut self, result: EditorResult) -> Result<()> {
        if !result.accepted {
            self.pending_add_target = None;
            self.pending_add_placement = AddPlacement::Natural;
            self.status = "Edit cancelled".into();
            return Ok(());
        }
        match result.purpose {
            EditorPurpose::Add => {
                let (expression, description) = split_task_editor_text(&result.text);
                if expression.is_empty() {
                    self.pending_add_target = None;
                    self.pending_add_placement = AddPlacement::Natural;
                    self.status = "Empty task was not added".into();
                    return Ok(());
                }
                let special_project = self.pending_add_target.take();
                let placement = std::mem::take(&mut self.pending_add_placement);
                let special_project_name =
                    special_project.as_ref().map(|project| project.name.clone());
                let from_virtual_view = special_project.is_none()
                    && matches!(
                        self.selected_nav(),
                        Some(NavId::Filter(_) | NavId::Today | NavId::Upcoming)
                    );
                let project = if let Some(project) = special_project {
                    project
                } else if let Some(project) = self.editor_add_fallback.clone() {
                    project
                } else {
                    self.add_target_project()?
                        .context("selected project and Inbox disappeared")?
                };
                let id = match placement {
                    AddPlacement::Natural => {
                        self.store
                            .add_task_with_description(expression, &description, &project)?
                    }
                    AddPlacement::Top => self.store.add_task_at_top_with_description(
                        expression,
                        &description,
                        &project,
                    )?,
                    AddPlacement::Bottom => self.store.add_task_at_bottom_with_description(
                        expression,
                        &description,
                        &project,
                    )?,
                };
                self.editor_mutation_committed = true;
                self.record_history(HistoryEntry::Add {
                    expression: expression.to_owned(),
                    description,
                    project,
                    parent_id: None,
                    task_id: id.clone(),
                    placement,
                });
                // Adding must not steal the user's place in the current view.
                // An empty view still selects the new task naturally because
                // there is no remembered selection to restore.
                self.refresh_after_action(None)?;
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
            EditorPurpose::AddChild => {
                let (expression, description) = split_task_editor_text(&result.text);
                if expression.is_empty() {
                    self.status = "Empty subtask was not added".into();
                    return Ok(());
                }
                let parent_id = result.task_id.context("missing parent task")?;
                let parent = self
                    .store
                    .task(&parent_id)?
                    .context("parent task is no longer available")?;
                let project = self
                    .store
                    .project(&parent.project_id)?
                    .context("parent task project is no longer available")?;
                let id = self.store.add_child_task_with_description(
                    expression,
                    &description,
                    &project,
                    &parent.id,
                )?;
                self.editor_mutation_committed = true;
                self.record_history(HistoryEntry::Add {
                    expression: expression.to_owned(),
                    description,
                    project,
                    parent_id: Some(parent.id),
                    task_id: id.clone(),
                    placement: AddPlacement::Natural,
                });
                self.refresh_after_action(None)?;
                self.focus = Focus::Tasks;
                self.status = "Subtask added · pending sync".into();
                self.mark_pending_and_sync();
            }
            EditorPurpose::Edit => {
                let id = result.task_id.context("missing edited task")?;
                let (title, description) = split_task_editor_text(&result.text);
                if title.is_empty() {
                    self.status = "Empty title: edit not applied".into();
                    return Ok(());
                }
                let before = self
                    .store
                    .task(&id)?
                    .context("edited task no longer exists")?;
                self.store.edit_task(&id, title, &description)?;
                self.editor_mutation_committed = true;
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
                self.editor_mutation_committed = true;
                self.record_task_change(before, &id, "due-date change")?;
                self.refresh_after_action(Some(&id))?;
                self.status = "Due date updated · pending sync".into();
                self.mark_pending_and_sync();
            }
            EditorPurpose::AddFilter => {
                let (name, query, is_favorite) = match parse_filter_editor(&result.text) {
                    Ok(value) => value,
                    Err(message) => {
                        self.status = message;
                        return Ok(());
                    }
                };
                self.store.add_filter(&name, &query, is_favorite)?;
                self.editor_mutation_committed = true;
                self.refresh_nav()?;
                self.force_tasks()?;
                self.status = format!("Filter ‘{name}’ added · pending sync");
                self.mark_pending_and_sync();
            }
            EditorPurpose::EditFilter => {
                let id = result.task_id.context("missing edited filter")?;
                let (name, query, is_favorite) = match parse_filter_editor(&result.text) {
                    Ok(value) => value,
                    Err(message) => {
                        self.status = message;
                        return Ok(());
                    }
                };
                self.store.update_filter(&id, &name, &query, is_favorite)?;
                self.editor_mutation_committed = true;
                self.refresh_nav()?;
                self.force_tasks()?;
                self.status = format!("Filter ‘{name}’ updated · pending sync");
                self.mark_pending_and_sync();
            }
            EditorPurpose::AddProject => {
                let name = match parse_project_editor(&result.text) {
                    Ok(name) => name,
                    Err(message) => {
                        self.status = message;
                        return Ok(());
                    }
                };
                let id = self.store.add_project(&name)?;
                self.editor_mutation_committed = true;
                self.refresh_nav()?;
                if let Some(index) = self.nav_rows.iter().position(
                    |row| matches!(row, NavRow::Item { id: NavId::Project(row_id), .. } if row_id == &id),
                ) {
                    self.select_nav(index);
                    self.force_tasks()?;
                }
                self.focus = Focus::Navigation;
                self.status = format!("Project ‘{name}’ added · pending sync");
                self.mark_pending_and_sync();
            }
            EditorPurpose::RenameProject => {
                let project_id = result.task_id.context("missing renamed project")?;
                let name = match parse_project_editor(&result.text) {
                    Ok(name) => name,
                    Err(message) => {
                        self.status = message;
                        return Ok(());
                    }
                };
                let id = self.store.rename_project(&project_id, &name)?;
                self.editor_mutation_committed = true;
                self.refresh_nav()?;
                if let Some(index) = self.nav_rows.iter().position(
                    |row| matches!(row, NavRow::Item { id: NavId::Project(row_id), .. } if row_id == &id),
                ) {
                    self.select_nav(index);
                }
                self.focus = Focus::Navigation;
                self.status = format!("Project renamed to ‘{name}’ · pending sync");
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
        let tree_before = self
            .store
            .tasks_for_view(&NavId::Project(before.project_id.clone()), true)?;
        let task_below = if completed {
            None
        } else {
            self.tasks.get(index + 1).map(|task| task.id.clone())
        };
        self.store.set_task_completed(&id, completed)?;
        self.record_tree_change(
            tree_before,
            if completed {
                "task completion"
            } else {
                "task reopening"
            },
            &id,
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

    fn delete_selected_task(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        let Some(task_id) = self.selected_task().map(|task| task.id.clone()) else {
            return Ok(());
        };
        self.delete_task(&task_id)
    }

    fn delete_task(&mut self, task_id: &str) -> Result<()> {
        let current_index = self.tasks.iter().position(|task| task.id == task_id);
        let next_task = current_index
            .and_then(|index| self.tasks.get(index + 1))
            .map(|task| task.id.clone())
            .or_else(|| {
                current_index
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| self.tasks.get(index))
                    .map(|task| task.id.clone())
            });
        let deleted = self.store.delete_task_tree(task_id)?;
        self.record_history(HistoryEntry::Delete { tasks: deleted });
        let nav = self.selected_nav().cloned();
        self.refresh_nav()?;
        if let Some(nav) = nav {
            if let Some(next_task) = next_task {
                self.remembered_tasks.insert(nav.clone(), next_task);
            } else {
                self.remembered_tasks.remove(&nav);
            }
            self.load_tasks(nav)?;
        }
        self.focus = Focus::Tasks;
        self.status = "Task deleted · pending sync".into();
        self.mark_pending_and_sync();
        Ok(())
    }

    fn remove_selected_due(&mut self) -> Result<()> {
        if self.focus != Focus::Tasks {
            return Ok(());
        }
        self.force_tasks()?;
        let Some(task_id) = self.selected_task().map(|task| task.id.clone()) else {
            return Ok(());
        };
        self.remove_due(&task_id)
    }

    fn remove_due(&mut self, task_id: &str) -> Result<()> {
        let before = self
            .store
            .task(task_id)?
            .context("dated task no longer exists")?;
        if before.due.is_none() {
            self.status = "Task has no due date".into();
            return Ok(());
        }
        self.store.update_due(task_id, "")?;
        self.record_task_change(before, task_id, "due-date removal")?;
        self.refresh_after_action(Some(task_id))?;
        self.focus = Focus::Tasks;
        self.status = "Due date removed · pending sync".into();
        self.mark_pending_and_sync();
        Ok(())
    }

    fn show_error_details(&mut self) {
        if let Some(message) = self.sync_issue.clone() {
            self.overlay = Some(Overlay::ErrorDetails { message, scroll: 0 });
        } else {
            self.status = "No synchronization error to show".into();
        }
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
            self.record_history(HistoryEntry::Change {
                before: vec![before],
                after: vec![after],
                action,
            });
        }
        Ok(())
    }

    fn record_tree_change(
        &mut self,
        mut snapshots: Vec<Task>,
        action: &'static str,
        root_id: &str,
    ) -> Result<()> {
        snapshots.sort_by_key(|task| task.id != root_id);
        let mut before = Vec::new();
        let mut after = Vec::new();
        for snapshot in snapshots {
            if let Some(current) = self.store.task(&snapshot.id)?
                && (current != snapshot
                    || (snapshot.id == root_id
                        && action == "task completion"
                        && snapshot.due.as_ref().is_some_and(|due| due.is_recurring)))
            {
                before.push(snapshot);
                after.push(current);
            }
        }
        if !before.is_empty() {
            self.record_history(HistoryEntry::Change {
                before,
                after,
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
            HistoryEntry::Add { task_id, .. } => {
                let mut ids = vec![task_id.clone()];
                if let Some(resolved) = self.store.resolve_task_id(task_id)? {
                    ids.push(resolved);
                }
                ids
            }
            HistoryEntry::Change { .. } | HistoryEntry::Delete { .. } => Vec::new(),
        };
        let preferred = match self.apply_history_entry(&mut entry, false) {
            Ok(preferred) => preferred,
            Err(error) => {
                self.redo_stack.push(entry);
                return Err(error);
            }
        };
        if let HistoryEntry::Add { task_id, .. } = &entry {
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
            HistoryEntry::Change {
                before,
                after,
                action,
            } => {
                let targets = if undo { before } else { after };
                if !undo
                    && *action == "task edit"
                    && let Some(target) = targets.first()
                {
                    self.store
                        .edit_task(&target.id, &target.content, &target.description)?;
                    return Ok(Some(
                        self.store
                            .task(&target.id)?
                            .context("edited task no longer exists")?
                            .id,
                    ));
                }
                let ids = self.store.apply_task_snapshots(
                    targets,
                    (*action == "task completion").then_some(undo),
                )?;
                Ok(ids.first().cloned())
            }
            HistoryEntry::Add { task_id, .. } if undo => {
                self.store.delete_task(task_id)?;
                Ok(None)
            }
            HistoryEntry::Add {
                expression,
                description,
                project,
                parent_id,
                task_id,
                placement,
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
                *task_id = if let Some(stored_parent_id) = parent_id.as_deref() {
                    let parent = self
                        .store
                        .task(stored_parent_id)?
                        .context("subtask parent no longer exists")?;
                    *parent_id = Some(parent.id.clone());
                    self.store.add_child_task_with_description(
                        expression,
                        description,
                        &destination,
                        &parent.id,
                    )?
                } else {
                    match placement {
                        AddPlacement::Natural => self.store.add_task_with_description(
                            expression,
                            description,
                            &destination,
                        )?,
                        AddPlacement::Top => self.store.add_task_at_top_with_description(
                            expression,
                            description,
                            &destination,
                        )?,
                        AddPlacement::Bottom => self.store.add_task_at_bottom_with_description(
                            expression,
                            description,
                            &destination,
                        )?,
                    }
                };
                *project = destination;
                Ok(Some(task_id.clone()))
            }
            HistoryEntry::Delete { tasks } if undo => {
                *tasks = self.store.restore_deleted_tasks(tasks)?;
                Ok(tasks.first().map(|task| task.id.clone()))
            }
            HistoryEntry::Delete { tasks } => {
                let root_id = tasks
                    .first()
                    .context("deleted task history is empty")?
                    .id
                    .clone();
                *tasks = self.store.delete_task_tree(&root_id)?;
                Ok(None)
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

    fn move_selected_item(&mut self, down: bool) -> Result<()> {
        match self.focus {
            Focus::Navigation => self.move_selected_project(down),
            Focus::Tasks => self.move_selected_task(down),
        }
    }

    fn move_selected_project(&mut self, down: bool) -> Result<()> {
        let Some(NavId::Project(id)) = self.selected_nav().cloned() else {
            self.status = "Only projects can be reordered".into();
            return Ok(());
        };
        let Some(project) = self.store.project(&id)? else {
            self.status = "Project is no longer available".into();
            return Ok(());
        };
        if project.is_inbox {
            self.status = "Inbox order is fixed".into();
            return Ok(());
        }
        if !self.store.move_project(&id, down)? {
            self.status = format!(
                "{} is already {} in this project group",
                project.name,
                if down { "last" } else { "first" }
            );
            return Ok(());
        }
        self.refresh_nav()?;
        self.status = format!(
            "{} moved {} · pending sync",
            project.name,
            if down { "down" } else { "up" }
        );
        self.mark_pending_and_sync();
        Ok(())
    }

    fn move_selected_task(&mut self, down: bool) -> Result<()> {
        self.force_tasks()?;
        let Some(selected) = self.selected_task().cloned() else {
            self.status = "No task selected".into();
            return Ok(());
        };
        if selected.checked {
            self.status = "Completed tasks cannot be reordered".into();
            return Ok(());
        }
        if self.sort_tasks_alphabetically {
            self.status = "Disable alphabetical sorting before reordering tasks".into();
            return Ok(());
        }

        let selected_group = selected.display_group().map(str::to_lowercase);
        let selected_date = selected
            .due
            .as_ref()
            .and_then(crate::model::Due::concrete_date);
        let upcoming = matches!(self.selected_nav(), Some(NavId::Upcoming));
        let candidates = self
            .tasks
            .iter()
            .filter(|task| {
                if task.checked
                    || task.project_id != selected.project_id
                    || task.parent_id != selected.parent_id
                    || task.section_id != selected.section_id
                {
                    return false;
                }
                if selected.parent_id.is_some() {
                    return true;
                }
                if upcoming {
                    return task.due.as_ref().and_then(crate::model::Due::concrete_date)
                        == selected_date;
                }
                !self.group_tasks_by_label
                    || task.display_group().map(str::to_lowercase) == selected_group
            })
            .collect::<Vec<_>>();
        let Some(position) = candidates.iter().position(|task| task.id == selected.id) else {
            self.status = "Selected task is not in the current order".into();
            return Ok(());
        };
        let target_position = if down {
            position
                .checked_add(1)
                .filter(|position| *position < candidates.len())
        } else {
            position.checked_sub(1)
        };
        let Some(target) = target_position.and_then(|position| candidates.get(position)) else {
            self.status = format!(
                "{} is already {} in this task group",
                selected.content,
                if down { "last" } else { "first" }
            );
            return Ok(());
        };
        let target_id = target.id.clone();
        self.store.reorder_task(&selected.id, &target_id)?;
        self.refresh_after_action(Some(&selected.id))?;
        self.status = format!(
            "{} moved {} · pending sync",
            selected.content,
            if down { "down" } else { "up" }
        );
        self.mark_pending_and_sync();
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
        } else if !self.sync.retry_after().is_zero() {
            self.status = format!(
                "Retrying synchronization in {} seconds",
                self.sync.retry_after().as_secs().saturating_add(1)
            );
        } else if self.store.retry_failed_operations().is_err() {
            self.status = "Could not prepare synchronization retry".into();
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
        if self.sync.is_running() {
            self.sync_follow_up_requested = true;
            self.sync_status = SyncStatus::Syncing;
            self.sync_issue = None;
            self.set_persistent_status("Syncing…");
        } else if self.start_sync_run() {
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
            SyncEvent::AuxiliaryWarning { message } => {
                self.sync_issue = Some(message);
                self.sync_status = SyncStatus::Error;
                self.set_persistent_status("Some cached views are stale");
            }
            SyncEvent::Started if self.scheduled_sync_running => {}
            SyncEvent::Started => {
                self.sync_status = SyncStatus::Syncing;
                self.sync_issue = None;
                self.set_persistent_status("Syncing…");
            }
            SyncEvent::Finished { pending } => {
                let pending = pending.max(self.store.outbox_len()?);
                let was_scheduled = self.scheduled_sync_running;
                self.scheduled_sync_running = false;
                self.sync_follow_up_requested = false;
                self.sync_issue = None;
                if self.sync.is_running() {
                    self.sync_status = SyncStatus::Syncing;
                } else if pending == 0 {
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
                if let Some(warning) = self
                    .store
                    .meta("auxiliary_warning")?
                    .filter(|warning| !warning.is_empty())
                {
                    self.sync_issue = Some(warning);
                    self.sync_status = SyncStatus::Error;
                    self.set_persistent_status("Some cached views are stale");
                }
            }
            SyncEvent::Failed { kind, message } => {
                self.scheduled_sync_running = false;
                if std::mem::take(&mut self.sync_follow_up_requested) && self.start_sync_run() {
                    self.sync_status = SyncStatus::Syncing;
                    self.sync_issue = None;
                    self.set_persistent_status("Syncing corrected changes…");
                    return Ok(());
                }
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

    pub fn report_runtime_error(&mut self, error: anyhow::Error) {
        // A failed editor or local command must not tear down the terminal.
        // The durable outbox is left intact and a later retry can recover.
        if !self.editor_is_open() {
            self.pending_add_target = None;
            self.pending_add_placement = AddPlacement::Natural;
            self.editor_add_fallback = None;
        }
        self.pending_render = None;
        let message = format!("Local error: {error:#}");
        self.sync_issue = Some(message.clone());
        self.sync_status = SyncStatus::Error;
        self.set_persistent_status(message);
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
            HistoryEntry::Change { before, after, .. } => {
                for task in before.iter_mut().chain(after.iter_mut()) {
                    if old_ids.contains(&task.id) {
                        task.id = new_id.to_owned();
                    }
                    if task
                        .parent_id
                        .as_ref()
                        .is_some_and(|parent| old_ids.contains(parent))
                    {
                        task.parent_id = Some(new_id.to_owned());
                    }
                }
            }
            HistoryEntry::Add {
                task_id, parent_id, ..
            } => {
                if old_ids.contains(task_id) {
                    *task_id = new_id.to_owned();
                }
                if parent_id
                    .as_ref()
                    .is_some_and(|parent| old_ids.contains(parent))
                {
                    *parent_id = Some(new_id.to_owned());
                }
            }
            HistoryEntry::Delete { tasks } => {
                for task in tasks {
                    if old_ids.contains(&task.id) {
                        task.id = new_id.to_owned();
                    }
                    if task
                        .parent_id
                        .as_ref()
                        .is_some_and(|parent| old_ids.contains(parent))
                    {
                        task.parent_id = Some(new_id.to_owned());
                    }
                }
            }
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

#[cfg(test)]
fn filtered_project_indices(projects: &[ProjectPickerItem], query: &str) -> Vec<usize> {
    ProjectSearch::default().matches(projects, query).to_vec()
}

pub fn filtered_label_indices(labels: &[LabelPickerItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    labels
        .iter()
        .enumerate()
        .filter_map(|(index, label)| label.search_name.starts_with(&query).then_some(index))
        .collect()
}

pub fn filtered_task_indices(tasks: &[TaskPickerItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| task.search_name.starts_with(&query).then_some(index))
        .collect()
}

pub fn filtered_filter_indices(filters: &[FilterPickerItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    filters
        .iter()
        .enumerate()
        .filter_map(|(index, filter)| filter.search_name.starts_with(&query).then_some(index))
        .collect()
}

fn parse_filter_editor(text: &str) -> std::result::Result<(String, String, bool), String> {
    let normalized = text.trim_end_matches(['\r', '\n']);
    let lines = normalized
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect::<Vec<_>>();
    if lines.len() != 3 {
        return Err(
            "Filter edit requires exactly three lines: title, query, favorite = true/false".into(),
        );
    }
    let name = lines[0].trim();
    let query = lines[1].trim();
    if name.is_empty() {
        return Err("Filter title cannot be empty".into());
    }
    if query.is_empty() {
        return Err("Filter query cannot be empty".into());
    }
    let Some((key, value)) = lines[2].split_once('=') else {
        return Err("Third line must be ‘favorite = true’ or ‘favorite = false’".into());
    };
    if !key.trim().eq_ignore_ascii_case("favorite") {
        return Err("Third line must begin with ‘favorite =’".into());
    }
    let is_favorite = match value.trim().to_ascii_lowercase().as_str() {
        "true" => true,
        "false" => false,
        _ => return Err("Favorite value must be true or false".into()),
    };
    Ok((name.to_owned(), query.to_owned(), is_favorite))
}

fn split_task_editor_text(text: &str) -> (&str, String) {
    let normalized = text.trim_end_matches(['\r', '\n']);
    let (title, description) = normalized.split_once('\n').unwrap_or((normalized, ""));
    (
        title.trim_end_matches('\r').trim(),
        description.replace("\r\n", "\n"),
    )
}

fn parse_project_editor(text: &str) -> std::result::Result<String, String> {
    let normalized = text.trim_end_matches(['\r', '\n']);
    if normalized.contains('\r') || normalized.contains('\n') {
        return Err("Project name must fit on one line".into());
    }
    let name = normalized.trim();
    if name.is_empty() {
        return Err("Project name cannot be empty".into());
    }
    Ok(name.to_owned())
}

fn order_tasks(
    tasks: &mut Vec<Task>,
    group_by_label: bool,
    alphabetically: bool,
    group_by_date: bool,
) {
    let original = std::mem::take(tasks);
    let ids = original
        .iter()
        .map(|task| task.id.clone())
        .collect::<HashSet<_>>();
    let original_order = original
        .iter()
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let metadata = original
        .iter()
        .map(|task| {
            (
                task.id.clone(),
                (
                    task.display_group().map(str::to_lowercase),
                    task.checked,
                    task.content.to_lowercase(),
                    task.due.as_ref().and_then(crate::model::Due::concrete_date),
                    task.child_order,
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut roots = Vec::new();
    let mut children = HashMap::<String, Vec<String>>::new();
    for task in &original {
        match task.parent_id.as_deref() {
            Some(parent_id) if parent_id != task.id && ids.contains(parent_id) => children
                .entry(parent_id.to_owned())
                .or_default()
                .push(task.id.clone()),
            _ => roots.push(task.id.clone()),
        }
    }

    if group_by_date {
        roots.sort_by_cached_key(|id| {
            let (_, checked, content, date, child_order) = &metadata[id];
            (
                *date,
                *checked,
                alphabetically.then(|| content.clone()),
                *child_order,
            )
        });
    } else {
        match (group_by_label, alphabetically) {
            (false, false) => {}
            (false, true) => roots.sort_by_cached_key(|id| {
                let (_, checked, content, _, _) = &metadata[id];
                (*checked, content.clone())
            }),
            (true, false) => roots.sort_by_cached_key(|id| {
                let (group, checked, _, _, _) = &metadata[id];
                (group.clone(), *checked)
            }),
            (true, true) => roots.sort_by_cached_key(|id| {
                let (group, checked, content, _, _) = &metadata[id];
                (group.clone(), *checked, content.clone())
            }),
        }
    }
    if alphabetically {
        for siblings in children.values_mut() {
            siblings.sort_by_cached_key(|id| metadata[id].2.clone());
        }
    }

    let mut remaining = original
        .into_iter()
        .map(|task| (task.id.clone(), task))
        .collect::<HashMap<_, _>>();
    for root in roots {
        append_task_tree(&root, &mut remaining, &children, tasks);
    }
    // Preserve every task even if remote data contains a parent cycle.
    for id in original_order {
        append_task_tree(&id, &mut remaining, &children, tasks);
    }
}

fn append_task_tree(
    id: &str,
    remaining: &mut HashMap<String, Task>,
    children: &HashMap<String, Vec<String>>,
    ordered: &mut Vec<Task>,
) {
    let Some(task) = remaining.remove(id) else {
        return;
    };
    ordered.push(task);
    if let Some(child_ids) = children.get(id) {
        for child_id in child_ids {
            append_task_tree(child_id, remaining, children, ordered);
        }
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
    use crate::model::Operation;

    fn task(id: &str, content: &str, labels: &[&str]) -> Task {
        Task {
            id: id.into(),
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
            section_id: None,
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
    fn task_search_opens_details_and_details_can_open_the_editor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        store.add_task("Alpha task", &inbox).unwrap();
        let expected_id = store.add_task("Beta task", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        for character in "beta".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            app.overlay.as_ref(),
            Some(Overlay::Details { task, .. }) if task.id == expected_id
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.overlay, Some(Overlay::Editor(_))));
    }

    #[test]
    fn task_details_can_open_the_due_date_editor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let task_id = store.add_task("Task with a date", &inbox).unwrap();
        let task = store.task(&task_id).unwrap().unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.overlay = Some(Overlay::Details { task, scroll: 0 });

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .unwrap();

        assert!(matches!(
            app.overlay.as_ref(),
            Some(Overlay::Editor(editor)) if editor.purpose() == EditorPurpose::Due
        ));
    }

    #[test]
    fn copy_title_uses_the_selection_in_the_focused_pane() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        store.add_task("Copy this task", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        let mut copied = String::new();
        app.copy_selected_title_with(|clipboard, title| {
            copied = title.to_owned();
            clipboard.start_with(title, |_, _| Ok(()))
        })
        .unwrap();
        assert_eq!(copied, "Inbox");
        finish_clipboard_copy(&mut app);
        assert_eq!(app.status, "Project or filter title copied");

        app.enter_tasks().unwrap();
        app.copy_selected_title_with(|clipboard, title| {
            copied = title.to_owned();
            clipboard.start_with(title, |_, _| Ok(()))
        })
        .unwrap();
        assert_eq!(copied, "Copy this task");
        finish_clipboard_copy(&mut app);
        assert_eq!(app.status, "Task title copied");
    }

    fn finish_clipboard_copy(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.clipboard.is_running() {
            app.tick().unwrap();
            assert!(Instant::now() < deadline, "clipboard worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_pending_clipboard_copy_keeps_navigation_and_quitting_responsive() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (_dir, mut app) = draft_test_app();
        let finish = Arc::new(AtomicBool::new(false));
        let worker_finish = finish.clone();
        app.copy_selected_title_with(|clipboard, title| {
            clipboard.start_with(title, move |text, cancel| {
                assert_eq!(text, "Inbox");
                while !worker_finish.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(app.poll_timeout(), SYNC_POLL_INTERVAL);
        let started = Instant::now();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_nav(), Some(&NavId::Today));
        app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.quit);
        assert!(started.elapsed() < Duration::from_secs(1));
        finish.store(true, Ordering::Release);
        finish_clipboard_copy(&mut app);
        assert_eq!(app.status, "Project or filter title copied");
    }

    #[test]
    fn a_clipboard_error_is_reported_without_exiting() {
        let (_dir, mut app) = draft_test_app();
        app.copy_selected_title_with(|clipboard, title| {
            clipboard.start_with(title, |_, _| anyhow::bail!("Clipboard timed out"))
        })
        .unwrap();
        finish_clipboard_copy(&mut app);
        assert_eq!(app.status, "Clipboard timed out");
        assert!(!app.quit);
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
        order_tasks(&mut normal, false, false, false);
        assert_eq!(task_ids(&normal), ["z", "work-b", "home-a", "work-a"]);

        let mut alphabetical = original.clone();
        order_tasks(&mut alphabetical, false, true, false);
        assert_eq!(task_ids(&alphabetical), ["home-a", "work-a", "work-b", "z"]);

        let mut grouped = original.clone();
        order_tasks(&mut grouped, true, false, false);
        assert_eq!(task_ids(&grouped), ["z", "home-a", "work-b", "work-a"]);

        let mut grouped_alphabetical = original;
        order_tasks(&mut grouped_alphabetical, true, true, false);
        assert_eq!(
            task_ids(&grouped_alphabetical),
            ["z", "home-a", "work-a", "work-b"]
        );
    }

    #[test]
    fn task_hierarchy_stays_together_and_only_siblings_sort_alphabetically() {
        let mut parent = task("parent", "Packing", &["Travel"]);
        parent.child_order = 0;
        let mut child_z = task("child-z", "Zebra item", &["Ignored"]);
        child_z.parent_id = Some(parent.id.clone());
        child_z.child_order = 2;
        let mut other = task("other", "Appointments", &["Home"]);
        other.child_order = 1;
        let mut child_a = task("child-a", "Adapter", &["Also ignored"]);
        child_a.parent_id = Some(parent.id.clone());
        child_a.child_order = 3;

        let mut tasks = vec![parent, other, child_z, child_a];
        order_tasks(&mut tasks, true, true, false);

        assert_eq!(task_ids(&tasks), ["other", "parent", "child-a", "child-z"]);
        assert_eq!(tasks[2].parent_id.as_deref(), Some("parent"));
        assert_eq!(tasks[3].parent_id.as_deref(), Some("parent"));
    }

    #[test]
    fn upcoming_order_is_chronological_and_alphabetical_within_each_date() {
        let today = Local::now().date_naive();
        let mut late = task("late", "Later day", &[]);
        late.due = Some(crate::model::Due {
            date: Some((today + chrono::Duration::days(1)).format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let mut zulu = task("zulu", "Zulu", &[]);
        zulu.due = Some(crate::model::Due {
            date: Some(today.format("%F").to_string()),
            ..crate::model::Due::default()
        });
        let mut alpha = task("alpha", "Alpha", &[]);
        alpha.due = zulu.due.clone();
        let mut tasks = vec![late, zulu, alpha];

        order_tasks(&mut tasks, false, true, true);

        assert_eq!(task_ids(&tasks), ["alpha", "zulu", "late"]);
    }

    #[test]
    fn configurable_five_row_navigation_moves_and_clamps_in_both_panes() {
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
                    depth: 0,
                };
                if index == 3 {
                    vec![NavRow::Separator, item]
                } else {
                    vec![item]
                }
            })
            .collect();
        app.nav_cursor = 0;

        app.move_down_by(5).unwrap();
        assert_eq!(
            app.selected_nav().cloned(),
            Some(NavId::Project("project-5".into()))
        );
        app.move_up_by(5).unwrap();
        assert_eq!(
            app.selected_nav().cloned(),
            Some(NavId::Project("project-0".into()))
        );

        app.focus = Focus::Tasks;
        app.tasks = (0..8)
            .map(|index| task(&index.to_string(), &format!("Task {index}"), &[]))
            .collect();
        app.task_cursor = Some(0);
        app.move_down_by(5).unwrap();
        assert_eq!(app.task_cursor, Some(5));
        app.move_down_by(5).unwrap();
        assert_eq!(app.task_cursor, Some(7));
        app.move_up_by(5).unwrap();
        assert_eq!(app.task_cursor, Some(2));
        app.move_up_by(5).unwrap();
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

            app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT))
                .unwrap();
            app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT))
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
    fn manual_add_placement_is_recorded_in_projects_even_when_sorted() {
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

        assert_eq!(
            app.effective_add_placement(AddPlacement::Top),
            AddPlacement::Top
        );
        assert_eq!(
            app.effective_add_placement(AddPlacement::Bottom),
            AddPlacement::Bottom
        );
        app.group_tasks_by_label = false;
        assert_eq!(
            app.effective_add_placement(AddPlacement::Top),
            AddPlacement::Top
        );
        assert_eq!(
            app.effective_add_placement(AddPlacement::Bottom),
            AddPlacement::Bottom
        );
        app.sort_tasks_alphabetically = true;
        assert_eq!(
            app.effective_add_placement(AddPlacement::Top),
            AddPlacement::Top
        );
        assert_eq!(
            app.effective_add_placement(AddPlacement::Bottom),
            AddPlacement::Bottom
        );

        app.sort_tasks_alphabetically = false;
        app.nav_cursor = app
            .nav_rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    NavRow::Item {
                        id: NavId::Today,
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(
            app.effective_add_placement(AddPlacement::Top),
            AddPlacement::Natural
        );
    }

    #[test]
    fn redo_preserves_top_addition_placement() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        store.add_task("Existing", &inbox).unwrap();
        let top_id = store.add_task_at_top("Top", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.record_history(HistoryEntry::Add {
            expression: "Top".into(),
            description: String::new(),
            project: inbox.clone(),
            parent_id: None,
            task_id: top_id,
            placement: AddPlacement::Top,
        });

        app.undo().unwrap();
        app.redo().unwrap();

        assert_eq!(
            store
                .tasks_for(&NavId::Project(inbox.id))
                .unwrap()
                .iter()
                .map(|task| task.content.as_str())
                .collect::<Vec<_>>(),
            ["Top", "Existing"]
        );
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
            text: "Pack passport\nPut it beside the door".into(),
        })
        .unwrap();
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.tasks[0].content, "Pack passport");
        assert_eq!(app.tasks[0].description, "Put it beside the door");
        assert!(app.store.outbox().unwrap().iter().any(|entry| matches!(
            &entry.operation,
            Operation::QuickAdd {
                expression,
                description,
                ..
            } if expression == "Pack passport" && description == "Put it beside the door"
        )));
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
        assert_eq!(app.tasks[0].description, "Put it beside the door");
        assert_ne!(app.tasks[0].id, original_id);
        assert_eq!(app.status, "Redid task addition · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.tasks[0].content, "Pack passport and tickets");
        assert_eq!(app.status, "Redid task edit · pending sync");
    }

    #[test]
    fn adding_a_task_preserves_the_previously_selected_task() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        store.add_task("First", &inbox).unwrap();
        let selected_id = store.add_task("Keep selected", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        let selected_index = app
            .tasks
            .iter()
            .position(|task| task.id == selected_id)
            .unwrap();
        app.set_task_cursor(selected_index);

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Add,
            task_id: None,
            accepted: true,
            text: "New task".into(),
        })
        .unwrap();

        assert_eq!(
            app.selected_task().map(|task| task.id.as_str()),
            Some(selected_id.as_str())
        );
        assert!(app.tasks.iter().any(|task| task.content == "New task"));
    }

    #[test]
    fn child_task_addition_can_be_undone_and_redone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let parent_id = store.add_task("Parent", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::AddChild,
            task_id: Some(parent_id.clone()),
            accepted: true,
            text: "Child".into(),
        })
        .unwrap();
        let child_id = app
            .tasks
            .iter()
            .find(|task| task.content == "Child")
            .unwrap()
            .id
            .clone();
        assert_eq!(
            app.selected_task().map(|task| task.id.as_str()),
            Some(parent_id.as_str())
        );
        assert_eq!(
            store.task(&child_id).unwrap().unwrap().parent_id.as_deref(),
            Some(parent_id.as_str())
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert!(store.task(&child_id).unwrap().is_none());
        assert_eq!(app.status, "Undid subtask addition · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        let redone = app.selected_task().unwrap();
        assert_eq!(redone.content, "Child");
        assert_eq!(redone.parent_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn task_tree_deletion_can_be_undone_and_redone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let parent_id = store.add_task("Parent", &inbox).unwrap();
        store.add_child_task("Child", &inbox, &parent_id).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.focus = Focus::Tasks;
        let parent_index = app
            .tasks
            .iter()
            .position(|task| task.id == parent_id)
            .unwrap();
        app.set_task_cursor(parent_index);

        app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.tasks.is_empty());
        assert!(
            store
                .outbox()
                .unwrap()
                .iter()
                .any(|entry| matches!(entry.operation, Operation::DeleteTask))
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.tasks[1].parent_id.as_deref(), Some(parent_id.as_str()));
        assert!(
            !store
                .outbox()
                .unwrap()
                .iter()
                .any(|entry| matches!(entry.operation, Operation::DeleteTask))
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.tasks.is_empty());
        assert_eq!(app.status, "Redid task deletion · pending sync");
    }

    #[test]
    fn completion_and_project_move_undo_restore_every_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        let parent = store.add_task("Parent", &inbox).unwrap();
        let child = store.add_child_task("Child", &inbox, &parent).unwrap();
        let grandchild = store.add_child_task("Grandchild", &inbox, &child).unwrap();
        let destination = store.add_project("Destination").unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.focus = Focus::Tasks;
        app.set_task_cursor(app.tasks.iter().position(|task| task.id == parent).unwrap());
        app.complete().unwrap();
        for id in [&parent, &child, &grandchild] {
            assert!(store.task(id).unwrap().unwrap().checked);
        }
        app.undo().unwrap();
        for id in [&parent, &child, &grandchild] {
            assert!(!store.task(id).unwrap().unwrap().checked);
        }
        app.apply_task_move(
            &parent,
            &ProjectPickerItem {
                id: destination.clone(),
                name: "Destination".into(),
            },
        )
        .unwrap();
        for id in [&parent, &child, &grandchild] {
            assert_eq!(store.task(id).unwrap().unwrap().project_id, destination);
        }
        app.undo().unwrap();
        for id in [&parent, &child, &grandchild] {
            assert_eq!(store.task(id).unwrap().unwrap().project_id, inbox.id);
        }
        assert_eq!(
            store
                .task(&grandchild)
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some(child.as_str())
        );
        app.redo().unwrap();
        for id in [&parent, &child, &grandchild] {
            assert_eq!(store.task(id).unwrap().unwrap().project_id, destination);
        }
    }

    #[test]
    fn earlier_edit_history_survives_synchronized_deletion_and_redo_reparses_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let remote = serde_json::from_value(
            serde_json::json!({"id":"task","project_id":"inbox","content":"Original"}),
        )
        .unwrap();
        store.merge_remote(&[], &[remote], &[], &[]).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.focus = Focus::Tasks;
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Edit,
            task_id: Some("task".into()),
            accepted: true,
            text: "Changed p1".into(),
        })
        .unwrap();
        let sent = store.claim_next_outbox().unwrap().unwrap();
        let mut parsed = store.task("task").unwrap().unwrap();
        parsed.content = "Changed".into();
        parsed.priority = 4;
        store.apply_synced_task_edit(&parsed, sent.seq).unwrap();
        store.finish_outbox(sent.seq).unwrap();
        app.force_tasks().unwrap();
        app.delete_task("task").unwrap();
        let deleted = store.claim_next_outbox().unwrap().unwrap();
        store.finish_outbox(deleted.seq).unwrap();
        app.undo().unwrap();
        let restored = store.task("task").unwrap().unwrap();
        assert_ne!(restored.id, "task");
        app.undo().unwrap();
        let current = store.task("task").unwrap().unwrap();
        assert_eq!(current.content, "Original");
        assert_eq!(current.priority, 1);
        assert!(
            store
                .outbox()
                .unwrap()
                .iter()
                .any(|entry| matches!(entry.operation, Operation::UpdatePriority { priority: 1 }))
        );
        app.redo().unwrap();
        assert!(store.outbox().unwrap().iter().any(|entry| matches!(&entry.operation, Operation::QuickEdit { expression, .. } if expression == "Changed p1")));
    }

    #[test]
    fn recurring_completion_stays_active_and_redo_does_not_advance_twice() {
        for (expression, interval) in [
            ("every day", 1),
            ("every 3 days", 3),
            ("every 2 weeks", 14),
            ("every week", 7),
            ("every! 3 days", 3),
            ("after 2 weeks", 14),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("cache.db")).unwrap();
            let today = Local::now().date_naive();
            let expected = Some(today + chrono::Duration::days(interval));
            let remote = serde_json::from_value(serde_json::json!({"id":"task","project_id":"inbox","content":"Recurring","due":{"date":today.format("%F").to_string(),"string":expression,"is_recurring":true}})).unwrap();
            store.merge_remote(&[], &[remote], &[], &[]).unwrap();
            let sync = SyncManager::new(store.clone(), None).unwrap();
            let mut app = App::new(
                store.clone(),
                sync,
                KeyBindings::defaults(),
                dir.path().join("config.toml"),
            )
            .unwrap();
            app.focus = Focus::Tasks;
            app.complete().unwrap();
            let next = store.task("task").unwrap().unwrap();
            assert!(!next.checked);
            assert_eq!(next.due.as_ref().unwrap().concrete_date(), expected);
            let sent = store.claim_next_outbox().unwrap().unwrap();
            store.finish_outbox(sent.seq).unwrap();
            app.undo().unwrap();
            assert_eq!(
                store
                    .task("task")
                    .unwrap()
                    .unwrap()
                    .due
                    .unwrap()
                    .concrete_date(),
                Some(today)
            );
            app.redo().unwrap();
            assert_eq!(
                store
                    .task("task")
                    .unwrap()
                    .unwrap()
                    .due
                    .unwrap()
                    .concrete_date(),
                expected
            );
            let queued = store.outbox().unwrap();
            assert!(
                queued
                    .iter()
                    .any(|entry| matches!(entry.operation, Operation::RestoreRecurring { .. }))
            );
            assert!(matches!(
                queued.last().unwrap().operation,
                Operation::CompleteTask
            ));
            assert!(!queued.iter().any(|entry| matches!(&entry.operation, Operation::RestoreDue { due: Some(due) } if due.concrete_date() == expected)));
        }
    }

    #[test]
    fn local_runtime_errors_leave_navigation_and_quitting_available() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.report_runtime_error(anyhow::anyhow!("could not start Neovim"));
        assert_eq!(app.sync_status, SyncStatus::Error);
        assert!(app.sync_issue.as_deref().unwrap().contains("Neovim"));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_nav(), Some(&NavId::Today));
        app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.quit);
    }

    #[test]
    fn due_date_removal_can_be_undone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let task_id = store.add_task("Dated", &inbox).unwrap();
        store.update_due(&task_id, "tomorrow").unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.focus = Focus::Tasks;

        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.selected_task().unwrap().due.is_none());
        assert_eq!(app.status, "Due date removed · pending sync");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.selected_task()
                .unwrap()
                .due
                .as_ref()
                .and_then(|due| due.string.as_deref()),
            Some("tomorrow")
        );
    }

    #[test]
    fn shifted_e_opens_the_complete_sync_error() {
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
        let message = "Sync error: complete diagnostic payload".to_owned();
        app.sync_issue = Some(message.clone());

        app.handle_key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT))
            .unwrap();

        assert!(matches!(
            app.overlay,
            Some(Overlay::ErrorDetails { message: ref shown, scroll: 0 }) if shown == &message
        ));
    }

    #[test]
    fn indent_and_outdent_support_arbitrary_task_depths_and_warn_at_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let first_id = store.add_task("First", &inbox).unwrap();
        let second_id = store.add_task("Second", &inbox).unwrap();
        let third_id = store.add_task("Third", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            store
                .task(&second_id)
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some(first_id.as_str())
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.selected_task().unwrap().id, third_id);
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            store.task(&third_id).unwrap().unwrap().parent_id.as_deref(),
            Some(second_id.as_str())
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            app.status,
            "Cannot indent: no preceding sibling can become the parent"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            store.task(&third_id).unwrap().unwrap().parent_id.as_deref(),
            Some(first_id.as_str())
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(store.task(&third_id).unwrap().unwrap().parent_id, None);
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(
            app.status,
            "Cannot outdent: task is already at the top level"
        );
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
    fn project_picker_ranks_case_insensitive_fuzzy_matches() {
        let projects = vec![
            ProjectPickerItem {
                id: "1".into(),
                name: "Inbox".into(),
            },
            ProjectPickerItem {
                id: "2".into(),
                name: "Shopping".into(),
            },
            ProjectPickerItem {
                id: "3".into(),
                name: "Shared work".into(),
            },
        ];
        assert_eq!(filtered_project_indices(&projects, "sh"), [1, 2]);
        assert_eq!(filtered_project_indices(&projects, "SHOP"), [1]);
        assert!(filtered_project_indices(&projects, "missing").is_empty());
    }

    #[test]
    fn fuzzy_project_matching_handles_icons_unicode_gaps_words_and_literal_punctuation() {
        let projects = [
            "#💼 Work",
            "Weekly review",
            "Project reports",
            "#✅ Team backlog",
            "👩‍💻 Entwicklung",
            "  #团队备忘",
            "!Important",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, name)| ProjectPickerItem {
            id: index.to_string(),
            name: name.into(),
        })
        .collect::<Vec<_>>();
        assert_eq!(filtered_project_indices(&projects, "wk"), [0, 1, 4]);
        assert_eq!(filtered_project_indices(&projects, "WORK"), [0]);
        assert_eq!(filtered_project_indices(&projects, "prj rpt"), [2]);
        assert_eq!(filtered_project_indices(&projects, "rpt prj"), [2]);
        assert_eq!(filtered_project_indices(&projects, "tbklg"), [3]);
        assert_eq!(filtered_project_indices(&projects, "ENTW"), [4]);
        assert_eq!(filtered_project_indices(&projects, "团队"), [5]);
        assert_eq!(filtered_project_indices(&projects, "!imp"), [6]);
        assert_eq!(filtered_project_indices(&projects, "💼"), [0]);
        assert!(filtered_project_indices(&projects, "workz").is_empty());
        assert_eq!(
            filtered_project_indices(&projects, "  "),
            (0..projects.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fuzzy_project_ranking_and_cache_update_for_confirm_backspace_and_reopening() {
        let projects = ["W---o---r---k", "Workbench", "Work", "Work"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| ProjectPickerItem {
                id: index.to_string(),
                name: name.into(),
            })
            .collect::<Vec<_>>();
        let mut search = ProjectSearch::default();
        let ranked = search.matches(&projects, "work").to_vec();
        assert_eq!(&ranked[..2], &[2, 3]);
        assert!(
            ranked.iter().position(|index| *index == 1).unwrap()
                < ranked.iter().position(|index| *index == 0).unwrap()
        );
        let cached = search.matches(&projects, "work").as_ptr();
        assert_eq!(search.matches(&projects, "work").as_ptr(), cached);
        assert!(search.matches(&projects, "workz").is_empty());
        assert_eq!(search.matches(&projects, "work"), ranked);
        assert_eq!(search.matches(&projects, ""), [0, 1, 2, 3]);
        let changed = vec![ProjectPickerItem {
            id: "new".into(),
            name: "Different".into(),
        }];
        assert!(
            ProjectSearch::default()
                .matches(&changed, "work")
                .is_empty()
        );
    }

    #[test]
    fn project_search_and_move_pick_hidden_team_names_by_fuzzy_text_without_icons() {
        for (name, query) in [
            ("#Team backlog", "tbklg"),
            ("💼 Team Work", "tm wk"),
            ("  #✅ Team reports", "tm rpt"),
            ("👩‍💻 团队备忘", "团备"),
        ] {
            for action in ['p', 'v'] {
                let (_dir, mut app) = draft_test_app();
                let projects = serde_json::from_value::<Vec<crate::model::RemoteProject>>(
                    serde_json::json!([{"id":"team","name":name,"workspace_id":"workspace"}]),
                )
                .unwrap();
                let tasks = serde_json::from_value::<Vec<crate::model::RemoteTask>>(
                    serde_json::json!([{"id":"source","project_id":"inbox","content":"Move this"}]),
                )
                .unwrap();
                app.store.merge_remote(&projects, &tasks, &[], &[]).unwrap();
                app.store.toggle_project_hidden("team").unwrap();
                app.refresh_nav().unwrap();
                app.enter_tasks().unwrap();
                app.handle_key(KeyEvent::new(KeyCode::Char(action), KeyModifiers::NONE))
                    .unwrap();
                for character in query.chars() {
                    app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                        .unwrap();
                }
                app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
                    .unwrap();
                if let Some(Overlay::ProjectPicker {
                    projects,
                    query,
                    search,
                    ..
                }) = app.overlay.as_mut()
                {
                    assert!(search.matches(projects, query).is_empty());
                } else {
                    panic!("project picker closed while typing");
                }
                app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                    .unwrap();
                // The confirmation must use the current ranked query even if
                // there has been no intervening render to populate its cache.
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                    .unwrap();
                assert!(app.overlay.is_none(), "{action}: {name}");
                assert_eq!(app.store.project("team").unwrap().unwrap().name, name);
                assert!(app.store.project("team").unwrap().unwrap().is_hidden);
                if action == 'v' {
                    assert_eq!(
                        app.store.task("source").unwrap().unwrap().project_id,
                        "team"
                    );
                } else {
                    assert_eq!(app.selected_nav(), Some(&NavId::Project("team".into())));
                    assert_eq!(app.store.outbox_len().unwrap(), 0);
                }
            }
        }
    }

    #[test]
    fn v_moves_task_trees_to_hidden_team_projects_and_prevents_unsupported_outbound_moves() {
        for (source, destination, prefix) in
            [("team", "personal", "per"), ("personal", "team", "tea")]
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("moves.db");
            let store = Store::open(&path).unwrap();
            let data = serde_json::json!({"projects":[
                {"id":"personal","name":"Personal","order_key":"a0"},
                {"id":"team","name":"Team","workspace_id":"workspace","default_order_key":"a0"}
            ],"items":[
                {"id":"root","project_id":source,"section_id":"source-section","content":"Root","order_key":"a0","description":"Keep this note","labels":["Keep"],"priority":4},
                {"id":"child","project_id":source,"section_id":"source-section","parent_id":"root","content":"Child","order_key":"a0"},
                {"id":"grandchild","project_id":source,"section_id":"source-section","parent_id":"child","content":"Grandchild","order_key":"a0"},
                {"id":"stay","project_id":source,"section_id":"source-section","content":"Stay","order_key":"a1"}
            ]});
            store.merge_order_metadata(&data, false).unwrap();
            store
                .merge_remote_snapshot(
                    &serde_json::from_value::<Vec<crate::model::RemoteProject>>(
                        data["projects"].clone(),
                    )
                    .unwrap(),
                    &serde_json::from_value::<Vec<crate::model::RemoteTask>>(data["items"].clone())
                        .unwrap(),
                    &[],
                    &[],
                    false,
                    None,
                )
                .unwrap();
            store.toggle_project_hidden(destination).unwrap();
            let mut app = App::new(
                store.clone(),
                SyncManager::new(store.clone(), None).unwrap(),
                KeyBindings::defaults(),
                dir.path().join("config.toml"),
            )
            .unwrap();
            assert!(!app.nav_rows.iter().any(
                |row| matches!(row,NavRow::Item{id:NavId::Project(id),..} if id==destination)
            ));
            let index = app
                .nav_rows
                .iter()
                .position(|row| matches!(row,NavRow::Item{id:NavId::Project(id),..} if id==source))
                .unwrap();
            app.select_nav(index);
            app.enter_tasks().unwrap();
            app.set_task_cursor(app.tasks.iter().position(|task| task.id == "root").unwrap());
            app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
                .unwrap();
            assert!(
                matches!(&app.overlay,Some(Overlay::ProjectPicker{projects,..}) if projects.iter().any(|project|project.id==destination) && !projects.iter().any(|project|project.id==source))
            );
            for character in prefix.chars() {
                app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                    .unwrap();
            }
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            assert!(app.overlay.is_none());
            assert_eq!(app.focus, Focus::Tasks);
            if source == "team" {
                assert!(
                    app.status
                        .contains("does not support moving individual tasks")
                );
                assert_eq!(store.outbox_len().unwrap(), 0);
                for id in ["root", "child", "grandchild"] {
                    assert_eq!(store.task(id).unwrap().unwrap().project_id, source);
                    assert_eq!(
                        store.task(id).unwrap().unwrap().section_id.as_deref(),
                        Some("source-section")
                    );
                }
                continue;
            }
            assert_eq!(app.selected_task().unwrap().id, "stay");
            for id in ["root", "child", "grandchild"] {
                let task = store.task(id).unwrap().unwrap();
                assert_eq!(task.project_id, destination);
                assert!(task.section_id.is_none());
            }
            assert_eq!(
                store.task("child").unwrap().unwrap().parent_id.as_deref(),
                Some("root")
            );
            assert_eq!(
                store
                    .task("grandchild")
                    .unwrap()
                    .unwrap()
                    .parent_id
                    .as_deref(),
                Some("child")
            );
            assert_eq!(
                store.task("root").unwrap().unwrap().description,
                "Keep this note"
            );
            assert_eq!(store.task("root").unwrap().unwrap().labels, ["Keep"]);
            assert_eq!(store.task("root").unwrap().unwrap().priority, 4);
            assert!(store.project(destination).unwrap().unwrap().is_hidden);
            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
                .unwrap();
            for id in ["root", "child", "grandchild"] {
                let task = store.task(id).unwrap().unwrap();
                assert_eq!(task.project_id, source);
                assert_eq!(task.section_id.as_deref(), Some("source-section"));
            }
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
                .unwrap();
            drop(app);
            drop(store);
            let reopened = Store::open(path).unwrap();
            assert_eq!(reopened.task("stay").unwrap().unwrap().project_id, source);
            for id in ["root", "child", "grandchild"] {
                assert_eq!(reopened.task(id).unwrap().unwrap().project_id, destination);
            }
            assert!(reopened.outbox().unwrap().iter().any(|entry|matches!(&entry.operation,crate::model::Operation::MoveTask{project_id} if project_id==destination)));
        }
    }

    #[test]
    #[ignore = "manual project-search benchmark; run with --ignored --nocapture"]
    fn project_search_benchmark() {
        let projects = (0..10_000)
            .map(|index| ProjectPickerItem {
                id: index.to_string(),
                name: format!("#💼 Team project {index} reports"),
            })
            .collect::<Vec<_>>();
        let mut search = ProjectSearch::default();
        let started = Instant::now();
        for _ in 0..20 {
            for query in ["tm prj", "rpt 42", "PROJECT", "tm rpt 9"] {
                std::hint::black_box(search.matches(&projects, query));
            }
        }
        eprintln!(
            "10,000 projects / 80 fuzzy query updates: {:?}",
            started.elapsed()
        );
        let started = Instant::now();
        for _ in 0..2000 {
            std::hint::black_box(search.matches(&projects, "tm rpt 9"));
        }
        eprintln!("2,000 cached result accesses: {:?}", started.elapsed());
    }

    #[test]
    fn moving_a_child_from_personal_to_team_detaches_only_that_subtree() {
        let (_dir, mut app) = draft_test_app();
        let projects:Vec<crate::model::RemoteProject>=serde_json::from_value(serde_json::json!([
            {"id":"personal","name":"Personal"},{"id":"team","name":"Team","workspace_id":"workspace"}
        ])).unwrap();
        let tasks: Vec<crate::model::RemoteTask> = serde_json::from_value(serde_json::json!([
            {"id":"root","project_id":"personal","content":"Root"},
            {"id":"child","project_id":"personal","parent_id":"root","content":"Child"},
            {"id":"grandchild","project_id":"personal","parent_id":"child","content":"Grandchild"}
        ]))
        .unwrap();
        app.store.merge_remote(&projects, &tasks, &[], &[]).unwrap();
        app.refresh_nav().unwrap();
        let index = app
            .nav_rows
            .iter()
            .position(|row| matches!(row,NavRow::Item{id:NavId::Project(id),..} if id=="personal"))
            .unwrap();
        app.select_nav(index);
        app.enter_tasks().unwrap();
        app.set_task_cursor(
            app.tasks
                .iter()
                .position(|task| task.id == "child")
                .unwrap(),
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
            .unwrap();
        for character in "tea".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.store.task("root").unwrap().unwrap().project_id,
            "personal"
        );
        assert_eq!(app.store.task("child").unwrap().unwrap().project_id, "team");
        assert!(
            app.store
                .task("child")
                .unwrap()
                .unwrap()
                .parent_id
                .is_none()
        );
        assert_eq!(
            app.store.task("grandchild").unwrap().unwrap().project_id,
            "team"
        );
        assert_eq!(
            app.store
                .task("grandchild")
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some("child")
        );
        app.undo().unwrap();
        assert_eq!(
            app.store
                .task("child")
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some("root")
        );
        assert_eq!(
            app.store.task("grandchild").unwrap().unwrap().project_id,
            "personal"
        );
    }

    #[test]
    fn v_recovers_an_old_rejected_workspace_move_and_successful_sync_clears_its_message() {
        let (_dir, mut app) = draft_test_app();
        let projects=serde_json::from_value::<Vec<crate::model::RemoteProject>>(serde_json::json!([
            {"id":"team","name":"Team","workspace_id":"workspace"},{"id":"personal","name":"Personal"}
        ])).unwrap();
        let tasks=serde_json::from_value::<Vec<crate::model::RemoteTask>>(serde_json::json!([
            {"id":"root","project_id":"team","content":"Root"},{"id":"child","project_id":"team","parent_id":"root","content":"Child"}
        ])).unwrap();
        app.store.merge_remote(&projects, &tasks, &[], &[]).unwrap();
        app.store.move_task("root", "personal").unwrap();
        let failed = app.store.claim_next_outbox().unwrap().unwrap();
        let message = "Moving from a workspace is not implemented.";
        app.store.fail_outbox(failed.seq, message).unwrap();
        app.store.toggle_project_hidden("team").unwrap();
        app.refresh_nav().unwrap();
        let index = app
            .nav_rows
            .iter()
            .position(|row| matches!(row,NavRow::Item{id:NavId::Project(id),..} if id=="personal"))
            .unwrap();
        app.select_nav(index);
        app.enter_tasks().unwrap();
        app.sync_issue = Some(message.into());
        app.sync_status = SyncStatus::Error;
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
            .unwrap();
        for character in "tea".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.store.task("root").unwrap().unwrap().project_id, "team");
        assert_eq!(app.store.task("child").unwrap().unwrap().project_id, "team");
        assert!(app.store.project("team").unwrap().unwrap().is_hidden);
        assert!(app.store.outbox_error().unwrap().is_none());
        assert!(app.store.outbox_len().unwrap() > 0);
        // The stale error is replaced by actual successful synchronization, not
        // by dismissing a still-unsent change or deleting its cached task.
        while let Some(entry) = app.store.next_outbox().unwrap() {
            app.store.finish_outbox(entry.seq).unwrap();
        }
        app.handle_sync_event(SyncEvent::Finished { pending: 0 })
            .unwrap();
        assert!(app.sync_issue.is_none());
        assert_eq!(app.sync_status, SyncStatus::Synced);
    }

    #[test]
    fn filter_picker_searches_all_names_and_filter_editor_is_strict() {
        let filters = ["Today and overdue", "Waiting", "Work"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| FilterPickerItem {
                filter: Filter {
                    id: index.to_string(),
                    name: name.into(),
                    query: "today".into(),
                    item_order: index as i64,
                    is_favorite: index == 0,
                    workspace_id: None,
                    is_frozen: false,
                },
                search_name: name.to_lowercase(),
            })
            .collect::<Vec<_>>();
        assert_eq!(filtered_filter_indices(&filters, "wa"), [1]);
        assert_eq!(filtered_filter_indices(&filters, "WORK"), [2]);

        assert_eq!(
            parse_filter_editor("My filter\n(today | overdue) & #Work\nFAVORITE = False\n"),
            Ok((
                "My filter".into(),
                "(today | overdue) & #Work".into(),
                false
            ))
        );
        assert!(parse_filter_editor("Missing query\n\nfavorite = true").is_err());
        assert!(parse_filter_editor("Title\ntoday\nfavorite = perhaps").is_err());
    }

    fn draft_test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("drafts.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        (dir, app)
    }

    fn save_reopened_draft(app: &mut App) -> EditorResult {
        let Some(Overlay::Editor(mut editor)) = app.overlay.take() else {
            panic!("draft did not reopen")
        };
        assert!(editor.notice().is_some());
        std::thread::sleep(Duration::from_millis(200));
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = editor.poll().unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "reopened draft did not save");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn invalid_filter_submission_reopens_the_exact_draft_without_committing() {
        let (_dir, mut app) = draft_test_app();
        let text = "Focus\n(today | overdue) & p1\nfavorite = perhaps\n";
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::AddFilter,
            task_id: None,
            accepted: true,
            text: text.into(),
        })
        .unwrap();
        assert!(app.store.filters().unwrap().is_empty());
        assert_eq!(app.store.outbox_len().unwrap(), 0);
        let mut submitted = save_reopened_draft(&mut app);
        assert_eq!(submitted.text, text);
        assert_eq!(submitted.purpose, EditorPurpose::AddFilter);
        submitted.text = text.replace("perhaps", "false");
        app.apply_editor_result(submitted).unwrap();
        assert_eq!(app.store.filters().unwrap().len(), 1);
        assert_eq!(app.store.outbox_len().unwrap(), 1);
    }

    #[test]
    fn a_rejected_task_edit_keeps_its_target_through_id_replacement() {
        let (_dir, mut app) = draft_test_app();
        let inbox = app.store.project("inbox").unwrap().unwrap();
        let id = app.store.add_task("Original", &inbox).unwrap();
        let add_seq = app.store.next_outbox().unwrap().unwrap().seq;
        let text = "\nDo not lose this description\n";
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Edit,
            task_id: Some(id.clone()),
            accepted: true,
            text: text.into(),
        })
        .unwrap();
        let remote = serde_json::from_value(
            serde_json::json!({"id":"server-task", "project_id":"inbox", "content":"Original"}),
        )
        .unwrap();
        app.store.replace_task_id(&id, &remote).unwrap();
        app.store.finish_outbox(add_seq).unwrap();
        let mut submitted = save_reopened_draft(&mut app);
        assert_eq!(submitted.text, text);
        assert_eq!(submitted.task_id.as_deref(), Some(id.as_str()));
        submitted.text = format!("Corrected{text}");
        app.apply_editor_result(submitted).unwrap();
        let task = app.store.task("server-task").unwrap().unwrap();
        assert_eq!(task.content, "Corrected");
        assert_eq!(task.description, "Do not lose this description");
        assert_eq!(
            app.store.next_outbox().unwrap().unwrap().entity_id,
            "server-task"
        );
    }

    #[test]
    fn a_failed_add_retains_placement_and_destination_when_the_selected_view_changes() {
        let (dir, mut app) = draft_test_app();
        let projects =
            serde_json::from_value::<Vec<crate::model::RemoteProject>>(serde_json::json!([
                {"id":"original", "name":"Original"}, {"id":"other", "name":"Other"}
            ]))
            .unwrap();
        app.store.merge_remote(&projects, &[], &[], &[]).unwrap();
        app.refresh_nav().unwrap();
        let project = app.store.project("original").unwrap().unwrap();
        app.editor_add_fallback = Some(project.clone());
        app.pending_add_target = Some(project);
        app.pending_add_placement = AddPlacement::Top;
        let mut writer = rusqlite::Connection::open(dir.path().join("drafts.db")).unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Add,
            task_id: None,
            accepted: true,
            text: "New task\nDescription\n".into(),
        })
        .unwrap();
        assert_eq!(app.store.outbox_len().unwrap(), 0);
        assert_eq!(app.pending_add_placement, AddPlacement::Top);
        assert_eq!(app.pending_add_target.as_ref().unwrap().id, "original");
        transaction.rollback().unwrap();
        let index = app
            .nav_rows
            .iter()
            .position(
                |row| matches!(row, NavRow::Item { id: NavId::Project(id), .. } if id == "other"),
            )
            .unwrap();
        app.select_nav(index);
        let submitted = save_reopened_draft(&mut app);
        app.apply_editor_result(submitted).unwrap();
        let tasks = app
            .store
            .tasks_for(&NavId::Project("original".into()))
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].content, "New task");
        assert_eq!(tasks[0].child_order, 0);
        assert!(
            app.store
                .tasks_for(&NavId::Project("other".into()))
                .unwrap()
                .is_empty()
        );
        assert_eq!(app.store.outbox_len().unwrap(), 1);
    }

    #[test]
    fn a_committed_add_is_not_reopened_when_the_view_refresh_fails() {
        let (dir, mut app) = draft_test_app();
        rusqlite::Connection::open(dir.path().join("drafts.db"))
            .unwrap()
            .execute("DROP TABLE filters", [])
            .unwrap();
        assert!(
            app.apply_editor_result(EditorResult {
                purpose: EditorPurpose::Add,
                task_id: None,
                accepted: true,
                text: "Only once\n".into()
            })
            .is_err()
        );
        assert!(!matches!(
            app.overlay,
            Some(Overlay::Editor(_) | Overlay::EditorRecovery { .. })
        ));
        assert_eq!(
            app.store
                .tasks_for(&NavId::Project("inbox".into()))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(app.store.outbox_len().unwrap(), 1);
    }

    #[test]
    fn an_invalid_project_draft_can_be_explicitly_cancelled() {
        let (_dir, mut app) = draft_test_app();
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::AddProject,
            task_id: None,
            accepted: true,
            text: "Two\nlines\n".into(),
        })
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.overlay.is_none());
        assert_eq!(app.store.projects().unwrap().len(), 1);
        assert_eq!(app.store.outbox_len().unwrap(), 0);
        assert!(app.editor_add_fallback.is_none());
    }

    #[test]
    fn an_unrelated_runtime_error_preserves_the_open_draft_and_its_add_context() {
        let (_dir, mut app) = draft_test_app();
        let inbox = app.store.project("inbox").unwrap().unwrap();
        app.pending_add_target = Some(inbox.clone());
        app.editor_add_fallback = Some(inbox);
        app.pending_add_placement = AddPlacement::Top;
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::Add,
            task_id: None,
            accepted: true,
            text: "\nRetained description\n".into(),
        })
        .unwrap();
        app.report_runtime_error(anyhow::anyhow!("background view refresh failed"));
        assert!(app.editor_is_open());
        assert_eq!(app.pending_add_placement, AddPlacement::Top);
        assert!(app.pending_add_target.is_some());
        assert!(app.editor_add_fallback.is_some());
        let mut submitted = save_reopened_draft(&mut app);
        assert_eq!(submitted.text, "\nRetained description\n");
        submitted.text.insert_str(0, "Recovered");
        app.apply_editor_result(submitted).unwrap();
        assert_eq!(app.store.outbox_len().unwrap(), 1);
        assert_eq!(
            app.store
                .tasks_for(&NavId::Project("inbox".into()))
                .unwrap()[0]
                .description,
            "Retained description"
        );
    }

    #[test]
    fn filter_add_edit_and_search_are_available_offline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::AddFilter,
            task_id: None,
            accepted: true,
            text: "Focus\ntoday & p1\nfavorite = true".into(),
        })
        .unwrap();
        let added = store.filters().unwrap().remove(0);
        assert!(app.nav_rows.iter().any(
            |row| matches!(row, NavRow::Item { id: NavId::Filter(id), .. } if id == &added.id)
        ));

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::EditFilter,
            task_id: Some(added.id.clone()),
            accepted: true,
            text: "Later\ntomorrow\nfavorite = false".into(),
        })
        .unwrap();
        let updated = store.filter(&added.id).unwrap().unwrap();
        assert_eq!(updated.name, "Later");
        assert!(!updated.is_favorite);
        assert!(!app.nav_rows.iter().any(
            |row| matches!(row, NavRow::Item { id: NavId::Filter(id), .. } if id == &added.id)
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(matches!(app.overlay, Some(Overlay::FilterPicker { .. })));
    }

    fn app_with_favorite_filter(inbox_has_tasks: bool) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let mut tasks = vec![
            serde_json::json!({"id":"filter-first", "project_id":"work", "content":"First filter task", "child_order":1}),
            serde_json::json!({"id":"filter-second", "project_id":"work", "content":"Second filter task", "child_order":2}),
        ];
        if inbox_has_tasks {
            tasks.extend([
                serde_json::json!({"id":"inbox-first", "project_id":"inbox", "content":"First Inbox task", "child_order":1}),
                serde_json::json!({"id":"inbox-second", "project_id":"inbox", "content":"Second Inbox task", "child_order":2}),
            ]);
        }
        let tasks: Vec<crate::model::RemoteTask> =
            serde_json::from_value(serde_json::Value::Array(tasks)).unwrap();
        store.merge_remote(
            &[serde_json::from_value(serde_json::json!({"id":"work", "name":"Work"})).unwrap()],
            &tasks, &[],
            &[serde_json::from_value(serde_json::json!({"id":"favorite", "name":"Focus", "query":"#Work", "is_favorite":true})).unwrap()],
        ).unwrap();
        store
            .replace_filter_membership("favorite", &tasks[..2])
            .unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        if inbox_has_tasks {
            let index = app
                .tasks
                .iter()
                .position(|task| task.id == "inbox-second")
                .unwrap();
            app.set_task_cursor(index);
        }
        let filter_index = app
            .nav_rows
            .iter()
            .position(
                |row| matches!(row, NavRow::Item { id: NavId::Filter(id), .. } if id == "favorite"),
            )
            .unwrap();
        app.select_nav(filter_index);
        app.force_tasks().unwrap();
        app.focus = Focus::Tasks;
        let index = app
            .tasks
            .iter()
            .position(|task| task.id == "filter-second")
            .unwrap();
        app.set_task_cursor(index);
        (dir, app)
    }

    fn save_filter_favorite(app: &mut App, is_favorite: bool) {
        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::EditFilter,
            task_id: Some("favorite".into()),
            accepted: true,
            text: format!("Edited filter\n#Work\nfavorite = {is_favorite}"),
        })
        .unwrap();
    }

    #[test]
    fn unfavoriting_current_filter_restores_destination_selection_and_cancels_stale_loads() {
        let (_dir, mut app) = app_with_favorite_filter(true);
        // A navigation load can still be pending while the editor is submitted.
        app.select_nav(app.nav_cursor);
        assert!(app.pending_render.is_some());
        save_filter_favorite(&mut app, false);

        assert_eq!(app.selected_nav(), Some(&NavId::Project("inbox".into())));
        assert_eq!(app.selected_nav_name(), "Inbox");
        assert_eq!(app.rendered_nav.as_ref(), app.selected_nav());
        assert!(app.tasks.iter().all(|task| task.project_id == "inbox"));
        assert_eq!(app.selected_task().unwrap().id, "inbox-second");
        assert!(app.pending_render.is_none());
        app.tick().unwrap();
        assert_eq!(app.selected_task().unwrap().id, "inbox-second");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(
            matches!(&app.overlay, Some(Overlay::Details { task, .. }) if task.id == "inbox-second")
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.store.task("inbox-second").unwrap().unwrap().checked);
        assert!(!app.store.task("filter-second").unwrap().unwrap().checked);
    }

    #[test]
    fn unfavoriting_current_filter_clears_tasks_for_an_empty_destination() {
        let (_dir, mut app) = app_with_favorite_filter(false);
        save_filter_favorite(&mut app, false);
        assert_eq!(app.selected_nav_name(), "Inbox");
        assert_eq!(app.rendered_nav.as_ref(), app.selected_nav());
        assert!(app.tasks.is_empty());
        assert!(app.task_cursor.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.overlay.is_none());
    }

    #[test]
    fn unfavoriting_the_last_visible_navigation_item_clears_the_task_pane() {
        let (_dir, mut app) = app_with_favorite_filter(false);
        for nav in [
            NavId::Project("inbox".into()),
            NavId::Today,
            NavId::Upcoming,
            NavId::Project("work".into()),
        ] {
            app.store.toggle_navigation_hidden(&nav).unwrap();
        }
        app.refresh_nav().unwrap();
        app.force_tasks().unwrap();
        assert_eq!(app.nav_rows.len(), 1);
        assert!(!app.tasks.is_empty());
        app.select_nav(app.nav_cursor);

        save_filter_favorite(&mut app, false);
        assert!(app.selected_nav().is_none());
        assert!(app.tasks.is_empty());
        assert!(app.task_cursor.is_none());
        assert!(app.rendered_nav.is_none());
        assert!(app.pending_render.is_none());
    }

    #[test]
    fn a_failed_destination_load_does_not_leave_old_filter_tasks_actionable() {
        let (dir, mut app) = app_with_favorite_filter(true);
        app.store
            .update_filter("favorite", "Focus", "#Work", false)
            .unwrap();
        app.refresh_nav().unwrap();
        assert_eq!(app.selected_nav_name(), "Inbox");
        // Simulate a storage failure after navigation has already changed.
        rusqlite::Connection::open(dir.path().join("todoist-tui.db"))
            .unwrap()
            .execute("DROP TABLE tasks", [])
            .unwrap();
        assert!(app.force_tasks().is_err());
        assert!(app.tasks.is_empty());
        assert!(app.task_cursor.is_none());
        assert!(app.rendered_nav.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.overlay.is_none());
    }

    #[test]
    fn editing_a_filter_that_remains_favorite_keeps_its_task_selection() {
        let (_dir, mut app) = app_with_favorite_filter(true);
        save_filter_favorite(&mut app, true);
        assert_eq!(app.selected_nav(), Some(&NavId::Filter("favorite".into())));
        assert_eq!(app.selected_nav_name(), "Edited filter");
        assert_eq!(app.selected_task().unwrap().id, "filter-second");
        assert_eq!(app.rendered_nav.as_ref(), app.selected_nav());
    }

    #[test]
    fn project_add_and_rename_are_available_offline() {
        assert_eq!(
            parse_project_editor("  Project name  \n"),
            Ok("Project name".into())
        );
        assert!(parse_project_editor("First\nSecond").is_err());
        assert!(parse_project_editor("   ").is_err());

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store.clone(),
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::AddProject,
            task_id: None,
            accepted: true,
            text: "New project".into(),
        })
        .unwrap();
        let project = store.project_named("New project").unwrap().unwrap();
        assert_eq!(
            app.selected_nav(),
            Some(&NavId::Project(project.id.clone()))
        );
        assert_eq!(app.focus, Focus::Navigation);

        app.apply_editor_result(EditorResult {
            purpose: EditorPurpose::RenameProject,
            task_id: Some(project.id.clone()),
            accepted: true,
            text: "Renamed project".into(),
        })
        .unwrap();
        assert_eq!(
            store.project(&project.id).unwrap().unwrap().name,
            "Renamed project"
        );
        assert_eq!(app.selected_nav_name(), "Renamed project");
    }

    #[test]
    fn project_picker_requires_two_cancels_to_close() {
        let mut mode = PickerMode::Filtering;

        assert!(!mode.cancel());
        assert_eq!(mode, PickerMode::Selecting);
        assert!(mode.cancel());
    }

    #[test]
    fn project_search_finds_hidden_projects_and_focuses_their_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let hidden_id = store.add_project("Hidden destination").unwrap();
        assert!(store.toggle_project_hidden(&hidden_id).unwrap());
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();

        assert!(!app.show_hidden_projects);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .unwrap();
        for character in "hidden".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        assert!(app.show_hidden_projects);
        assert_eq!(app.selected_nav(), Some(&NavId::Project(hidden_id)));
        assert_eq!(app.focus, Focus::Tasks);
        assert_eq!(app.status, "Jumped to Hidden destination");
    }

    #[test]
    fn project_reordering_keeps_the_moved_project_selected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let first_id = store.add_project("First").unwrap();
        store.add_project("Second").unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        let first_index = app
            .nav_rows
            .iter()
            .position(
                |row| matches!(row, NavRow::Item { id: NavId::Project(id), .. } if id == &first_id),
            )
            .unwrap();
        app.select_nav(first_index);

        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
            .unwrap();

        assert_eq!(app.selected_nav(), Some(&NavId::Project(first_id)));
        assert_eq!(app.status, "First moved down · pending sync");
        let project_names = app
            .nav_rows
            .iter()
            .filter_map(|row| match row {
                NavRow::Item {
                    id: NavId::Project(_),
                    name,
                    is_inbox: false,
                    ..
                } => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(project_names, ["Second", "First"]);
    }

    #[test]
    fn contextual_reordering_moves_tasks_when_tasks_have_focus() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("todoist-tui.db")).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let first_id = store.add_task("First", &inbox).unwrap();
        store.add_task("Second", &inbox).unwrap();
        let sync = SyncManager::new(store.clone(), None).unwrap();
        let mut app = App::new(
            store,
            sync,
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.focus = Focus::Tasks;
        let first_index = app
            .tasks
            .iter()
            .position(|task| task.id == first_id)
            .unwrap();
        app.set_task_cursor(first_index);

        app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT))
            .unwrap();

        assert_eq!(app.selected_task().unwrap().id, first_id);
        assert_eq!(
            app.tasks
                .iter()
                .map(|task| task.content.as_str())
                .collect::<Vec<_>>(),
            ["Second", "First"]
        );
        assert_eq!(app.status, "First moved down · pending sync");
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
    fn help_scrolls_with_navigation_keys_and_closes_normally() {
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

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 0 })));

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 1 })));
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 0 })));

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.overlay.is_none());
    }

    #[test]
    fn task_filter_and_error_details_use_configured_scroll_and_close_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[keybindings]\ndown = ['ctrl+n']\nup = ['ctrl+p']\nclose_dialog = ['esc']\n",
        )
        .unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        let id = store.add_task("Details", &inbox).unwrap();
        let task = store.task(&id).unwrap().unwrap();
        let filter_id = store.add_filter("Filter", "today", true).unwrap();
        let filter = store.filter(&filter_id).unwrap().unwrap();
        let mut app = App::new(
            store.clone(),
            SyncManager::new(store, None).unwrap(),
            KeyBindings::load(&path, true).unwrap(),
            path,
        )
        .unwrap();
        for overlay in [
            Overlay::Details { task, scroll: 0 },
            Overlay::FilterDetails { filter, scroll: 0 },
            Overlay::ErrorDetails {
                message: "Error".into(),
                scroll: 0,
            },
        ] {
            app.overlay = Some(overlay);
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
                .unwrap();
            assert!(matches!(
                app.overlay,
                Some(
                    Overlay::Details { scroll: 1, .. }
                        | Overlay::FilterDetails { scroll: 1, .. }
                        | Overlay::ErrorDetails { scroll: 1, .. }
                )
            ));
            app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .unwrap();
            app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .unwrap();
            assert!(matches!(
                app.overlay,
                Some(
                    Overlay::Details { scroll: 0, .. }
                        | Overlay::FilterDetails { scroll: 0, .. }
                        | Overlay::ErrorDetails { scroll: 0, .. }
                )
            ));
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            assert!(app.overlay.is_none());
        }
    }

    fn await_outbox(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.outbox_load.is_some() {
            app.tick().unwrap();
            assert!(
                Instant::now() < deadline,
                "diagnostics worker did not finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn local_outbox_loads_pages_opens_details_and_refreshes_without_syncing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        for index in 0..55 {
            store.add_task(&format!("Task {index}"), &inbox).unwrap();
        }
        let mut app = App::new(
            store.clone(),
            SyncManager::new(store.clone(), None).unwrap(),
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(matches!(
            app.overlay,
            Some(Overlay::Outbox(OutboxView { loading: true, .. }))
        ));
        await_outbox(&mut app);
        assert!(
            matches!(&app.overlay,Some(Overlay::Outbox(view)) if view.page.as_ref().unwrap().entries.len()==50)
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .unwrap();
        await_outbox(&mut app);
        assert!(
            matches!(&app.overlay,Some(Overlay::Outbox(view)) if view.page.as_ref().unwrap().offset==50 && view.page.as_ref().unwrap().entries.len()==5)
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .unwrap();
        await_outbox(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        let selected = match &app.overlay {
            Some(Overlay::Outbox(view)) => view.page.as_ref().unwrap().entries[view.cursor].seq,
            _ => panic!(),
        };
        store
            .finish_outbox(store.next_outbox().unwrap().unwrap().seq)
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        await_outbox(&mut app);
        assert!(!app.sync.is_running());
        assert!(
            matches!(&app.overlay,Some(Overlay::Outbox(view)) if view.cursor==0 && view.page.as_ref().unwrap().entries[0].seq==selected)
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert!(
            matches!(&app.overlay,Some(Overlay::Outbox(view)) if view.details && view.scroll==1)
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(&app.overlay,Some(Overlay::Outbox(view)) if !view.details));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.overlay.is_none());
        assert_eq!(store.outbox_len().unwrap(), 54);
    }

    #[test]
    fn obsolete_diagnostics_results_do_not_replace_unrelated_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let mut app = App::new(
            store.clone(),
            SyncManager::new(store, None).unwrap(),
            KeyBindings::defaults(),
            dir.path().join("config.toml"),
        )
        .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        await_outbox(&mut app);
        assert!(matches!(app.overlay, Some(Overlay::Help { .. })));
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

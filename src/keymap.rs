use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::editor::EditorOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Down,
    Up,
    DownFive,
    UpFive,
    FocusNavigation,
    FocusTasks,
    Confirm,
    SearchProjects,
    SearchTasks,
    SearchFilters,
    AddFilter,
    RenameProject,
    AddProject,
    MoveProjectDown,
    MoveProjectUp,
    CopyTitle,
    EditTask,
    CompleteTask,
    DeleteTask,
    MoveTask,
    AddChildTask,
    IndentTask,
    OutdentTask,
    EditDue,
    RemoveDue,
    ShowError,
    AddTaskTop,
    AddTaskBottom,
    Quit,
    First,
    Last,
    Jump,
    Cancel,
    Sync,
    Undo,
    Redo,
    ToggleLabelGrouping,
    ToggleAlphabeticalSort,
    ToggleCompletedTasks,
    ToggleHiddenProjects,
    ToggleProjectHidden,
    Space0,
    Space1,
    Space2,
    Space3,
    Space4,
    Space5,
    Space6,
    Space7,
    Space8,
    Space9,
    EditLabels,
    Help,
    CloseDialog,
}

impl Action {
    const ALL: [Self; 53] = [
        Self::Down,
        Self::Up,
        Self::DownFive,
        Self::UpFive,
        Self::FocusNavigation,
        Self::FocusTasks,
        Self::Confirm,
        Self::SearchProjects,
        Self::SearchTasks,
        Self::SearchFilters,
        Self::AddFilter,
        Self::RenameProject,
        Self::AddProject,
        Self::MoveProjectDown,
        Self::MoveProjectUp,
        Self::CopyTitle,
        Self::EditTask,
        Self::CompleteTask,
        Self::DeleteTask,
        Self::MoveTask,
        Self::AddChildTask,
        Self::IndentTask,
        Self::OutdentTask,
        Self::EditDue,
        Self::RemoveDue,
        Self::ShowError,
        Self::AddTaskTop,
        Self::AddTaskBottom,
        Self::Quit,
        Self::First,
        Self::Last,
        Self::Jump,
        Self::Cancel,
        Self::Sync,
        Self::Undo,
        Self::Redo,
        Self::ToggleLabelGrouping,
        Self::ToggleAlphabeticalSort,
        Self::ToggleCompletedTasks,
        Self::ToggleHiddenProjects,
        Self::ToggleProjectHidden,
        Self::Space0,
        Self::Space1,
        Self::Space2,
        Self::Space3,
        Self::Space4,
        Self::Space5,
        Self::Space6,
        Self::Space7,
        Self::Space8,
        Self::Space9,
        Self::EditLabels,
        Self::Help,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct KeyStroke {
    code: KeyCode,
    modifiers: KeyModifiers,
}

type Sequence = Vec<KeyStroke>;

#[derive(Debug, Clone)]
pub struct KeyBindings {
    parsed: HashMap<Action, Vec<Sequence>>,
    display: HashMap<Action, String>,
    main: HashMap<KeyStroke, Action>,
    special_add: Vec<SpecialAddBinding>,
    project_jumps: BTreeMap<String, String>,
    custom_jump_projects: HashSet<String>,
}

#[derive(Debug, Clone)]
struct SpecialAddBinding {
    stroke: KeyStroke,
    display: String,
    project: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    keybindings: BindingConfig,
    special_add: BTreeMap<String, String>,
    project_jumps: BTreeMap<String, String>,
    editor: EditorOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BindingConfig {
    down: Vec<String>,
    up: Vec<String>,
    down_five: Vec<String>,
    up_five: Vec<String>,
    focus_navigation: Vec<String>,
    focus_tasks: Vec<String>,
    confirm: Vec<String>,
    search_projects: Vec<String>,
    search_tasks: Vec<String>,
    search_filters: Vec<String>,
    add_filter: Vec<String>,
    rename_project: Vec<String>,
    add_project: Vec<String>,
    move_project_down: Vec<String>,
    move_project_up: Vec<String>,
    copy_title: Vec<String>,
    edit_task: Vec<String>,
    complete_task: Vec<String>,
    delete_task: Vec<String>,
    move_task: Vec<String>,
    add_child_task: Vec<String>,
    indent_task: Vec<String>,
    outdent_task: Vec<String>,
    edit_due: Vec<String>,
    remove_due: Vec<String>,
    show_error: Vec<String>,
    add_task_top: Vec<String>,
    add_task_bottom: Vec<String>,
    // Compatibility with the former single add action. When present, these
    // bindings retain the old bottom-append behavior and replace both new
    // add bindings.
    add_task: Option<Vec<String>>,
    quit: Vec<String>,
    first: Vec<String>,
    last: Vec<String>,
    jump: Vec<String>,
    cancel: Vec<String>,
    sync: Vec<String>,
    undo: Vec<String>,
    redo: Vec<String>,
    toggle_label_grouping: Vec<String>,
    toggle_alphabetical_sort: Vec<String>,
    toggle_completed_tasks: Vec<String>,
    toggle_hidden_projects: Vec<String>,
    toggle_project_hidden: Vec<String>,
    space_0: Vec<String>,
    space_1: Vec<String>,
    space_2: Vec<String>,
    space_3: Vec<String>,
    space_4: Vec<String>,
    space_5: Vec<String>,
    space_6: Vec<String>,
    space_7: Vec<String>,
    space_8: Vec<String>,
    space_9: Vec<String>,
    // Accepted for compatibility with configurations from the old three-group model.
    toggle_team: Vec<String>,
    toggle_third: Vec<String>,
    edit_labels: Vec<String>,
    help: Vec<String>,
    close_dialog: Vec<String>,
}

impl Default for BindingConfig {
    fn default() -> Self {
        Self {
            down: strings(&["j", "down"]),
            up: strings(&["k", "up"]),
            down_five: Vec::new(),
            up_five: Vec::new(),
            focus_navigation: strings(&["h"]),
            focus_tasks: strings(&["l"]),
            confirm: strings(&["enter"]),
            search_projects: strings(&["p"]),
            search_tasks: strings(&["f"]),
            search_filters: strings(&["shift+f"]),
            add_filter: strings(&["ctrl+f"]),
            rename_project: strings(&["r"]),
            add_project: strings(&["shift+p"]),
            move_project_down: strings(&["shift+j"]),
            move_project_up: strings(&["shift+k"]),
            copy_title: strings(&["y"]),
            edit_task: strings(&["e"]),
            complete_task: strings(&["d"]),
            delete_task: strings(&["shift+d"]),
            move_task: strings(&["v"]),
            add_child_task: strings(&["c"]),
            indent_task: strings(&[">"]),
            outdent_task: strings(&["<"]),
            edit_due: strings(&["t"]),
            remove_due: strings(&["shift+t"]),
            show_error: strings(&["shift+e"]),
            add_task_top: strings(&["a"]),
            add_task_bottom: strings(&["q"]),
            add_task: None,
            quit: strings(&["shift+q"]),
            first: strings(&["g g"]),
            last: strings(&["shift+g"]),
            jump: strings(&["g"]),
            cancel: strings(&["esc"]),
            sync: strings(&["r"]),
            undo: strings(&["u"]),
            redo: strings(&["ctrl+r"]),
            toggle_label_grouping: strings(&["shift+l"]),
            toggle_alphabetical_sort: strings(&["shift+a"]),
            toggle_completed_tasks: strings(&["shift+c"]),
            toggle_hidden_projects: strings(&["shift+i"]),
            toggle_project_hidden: strings(&["ctrl+i"]),
            space_0: strings(&["ctrl+0"]),
            space_1: strings(&["ctrl+1"]),
            space_2: strings(&["ctrl+2"]),
            space_3: strings(&["ctrl+3"]),
            space_4: strings(&["ctrl+4"]),
            space_5: strings(&["ctrl+5"]),
            space_6: strings(&["ctrl+6"]),
            space_7: strings(&["ctrl+7"]),
            space_8: strings(&["ctrl+8"]),
            space_9: strings(&["ctrl+9"]),
            toggle_team: Vec::new(),
            toggle_third: Vec::new(),
            edit_labels: strings(&["@"]),
            help: strings(&["?"]),
            close_dialog: strings(&["enter", "esc", "q"]),
        }
    }
}

impl KeyBindings {
    #[cfg(test)]
    pub fn load(path: &Path, required: bool) -> Result<Self> {
        Self::load_with_editor_options(path, required).map(|(bindings, _)| bindings)
    }

    pub fn load_with_editor_options(path: &Path, required: bool) -> Result<(Self, EditorOptions)> {
        if !path.exists() {
            if required {
                bail!("configuration file does not exist: {}", path.display());
            }
            let config = FileConfig::default();
            let editor = config.editor;
            return Ok((Self::from_file_config(config)?, editor));
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let config: FileConfig = toml::from_str(&source)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        let editor = config.editor;
        let bindings = Self::from_file_config(config)
            .with_context(|| format!("validate configuration in {}", path.display()))?;
        Ok((bindings, editor))
    }

    #[cfg(test)]
    pub fn defaults() -> Self {
        Self::from_file_config(FileConfig::default()).expect("built-in keybindings are valid")
    }

    pub fn matches(&self, action: Action, key: KeyEvent) -> bool {
        let stroke = KeyStroke::from_event(key);
        self.parsed.get(&action).is_some_and(|alternatives| {
            alternatives
                .iter()
                .any(|sequence| sequence.len() == 1 && sequence[0] == stroke)
        })
    }

    pub fn main_action(&self, key: KeyEvent) -> Option<Action> {
        self.main.get(&KeyStroke::from_event(key)).copied()
    }

    pub fn follows_jump(&self, action: Action, key: KeyEvent) -> bool {
        let stroke = KeyStroke::from_event(key);
        let Some(jumps) = self.parsed.get(&Action::Jump) else {
            return false;
        };
        self.parsed.get(&action).is_some_and(|alternatives| {
            alternatives.iter().any(|sequence| {
                sequence.len() == 2
                    && jumps
                        .iter()
                        .any(|jump| jump.len() == 1 && jump[0] == sequence[0])
                    && sequence[1] == stroke
            })
        })
    }

    pub fn display(&self, action: Action) -> &str {
        self.display.get(&action).map(String::as_str).unwrap_or("—")
    }

    pub fn special_add_project(&self, key: KeyEvent) -> Option<&str> {
        let stroke = KeyStroke::from_event(key);
        self.special_add
            .iter()
            .find(|binding| binding.stroke == stroke)
            .map(|binding| binding.project.as_str())
    }

    pub fn special_add_bindings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.special_add
            .iter()
            .map(|binding| (binding.display.as_str(), binding.project.as_str()))
    }

    pub fn project_jump_target(&self, prefix: &str) -> Option<&str> {
        self.project_jumps.get(prefix).map(String::as_str)
    }

    pub fn has_project_jump_with_prefix(&self, prefix: &str) -> bool {
        self.project_jumps
            .keys()
            .any(|configured| configured.starts_with(prefix))
    }

    pub fn project_uses_custom_jump(&self, project: &str) -> bool {
        self.custom_jump_projects.contains(&project.to_lowercase())
    }

    pub fn project_jump_bindings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.project_jumps
            .iter()
            .map(|(prefix, project)| (prefix.as_str(), project.as_str()))
    }

    fn from_file_config(config: FileConfig) -> Result<Self> {
        Self::from_parts(config.keybindings, config.special_add, config.project_jumps)
    }

    #[cfg(test)]
    fn from_config(config: BindingConfig) -> Result<Self> {
        Self::from_parts(config, BTreeMap::new(), BTreeMap::new())
    }

    fn from_parts(
        config: BindingConfig,
        special_add_config: BTreeMap<String, String>,
        project_jump_config: BTreeMap<String, String>,
    ) -> Result<Self> {
        drop(config.toggle_team);
        drop(config.toggle_third);
        let (add_task_top, add_task_bottom) = match config.add_task {
            Some(legacy) => (Vec::new(), legacy),
            None => (config.add_task_top, config.add_task_bottom),
        };
        let entries = [
            (Action::Down, config.down),
            (Action::Up, config.up),
            (Action::DownFive, config.down_five),
            (Action::UpFive, config.up_five),
            (Action::FocusNavigation, config.focus_navigation),
            (Action::FocusTasks, config.focus_tasks),
            (Action::Confirm, config.confirm),
            (Action::SearchProjects, config.search_projects),
            (Action::SearchTasks, config.search_tasks),
            (Action::SearchFilters, config.search_filters),
            (Action::AddFilter, config.add_filter),
            (Action::RenameProject, config.rename_project),
            (Action::AddProject, config.add_project),
            (Action::MoveProjectDown, config.move_project_down),
            (Action::MoveProjectUp, config.move_project_up),
            (Action::CopyTitle, config.copy_title),
            (Action::EditTask, config.edit_task),
            (Action::CompleteTask, config.complete_task),
            (Action::DeleteTask, config.delete_task),
            (Action::MoveTask, config.move_task),
            (Action::AddChildTask, config.add_child_task),
            (Action::IndentTask, config.indent_task),
            (Action::OutdentTask, config.outdent_task),
            (Action::EditDue, config.edit_due),
            (Action::RemoveDue, config.remove_due),
            (Action::ShowError, config.show_error),
            (Action::AddTaskTop, add_task_top),
            (Action::AddTaskBottom, add_task_bottom),
            (Action::Quit, config.quit),
            (Action::First, config.first),
            (Action::Last, config.last),
            (Action::Jump, config.jump),
            (Action::Cancel, config.cancel),
            (Action::Sync, config.sync),
            (Action::Undo, config.undo),
            (Action::Redo, config.redo),
            (Action::ToggleLabelGrouping, config.toggle_label_grouping),
            (
                Action::ToggleAlphabeticalSort,
                config.toggle_alphabetical_sort,
            ),
            (Action::ToggleCompletedTasks, config.toggle_completed_tasks),
            (Action::ToggleHiddenProjects, config.toggle_hidden_projects),
            (Action::ToggleProjectHidden, config.toggle_project_hidden),
            (Action::Space0, config.space_0),
            (Action::Space1, config.space_1),
            (Action::Space2, config.space_2),
            (Action::Space3, config.space_3),
            (Action::Space4, config.space_4),
            (Action::Space5, config.space_5),
            (Action::Space6, config.space_6),
            (Action::Space7, config.space_7),
            (Action::Space8, config.space_8),
            (Action::Space9, config.space_9),
            (Action::EditLabels, config.edit_labels),
            (Action::Help, config.help),
            (Action::CloseDialog, config.close_dialog),
        ];
        let mut parsed = HashMap::new();
        let mut display = HashMap::new();
        for (action, values) in entries {
            let sequences = values
                .iter()
                .map(|value| parse_sequence(value))
                .collect::<Result<Vec<_>>>()?;
            if action != Action::First && sequences.iter().any(|sequence| sequence.len() != 1) {
                bail!("only the 'first' binding may contain a multi-key sequence");
            }
            if action == Action::First && sequences.iter().any(|sequence| sequence.len() > 2) {
                bail!("the 'first' binding accepts at most two keys");
            }
            display.insert(
                action,
                if values.is_empty() {
                    "unbound".into()
                } else {
                    values.join(" / ")
                },
            );
            parsed.insert(action, sequences);
        }
        let mut bindings = Self {
            parsed,
            display,
            main: HashMap::new(),
            special_add: Vec::new(),
            project_jumps: BTreeMap::new(),
            custom_jump_projects: HashSet::new(),
        };
        bindings.validate_first_sequence()?;
        bindings.validate_main_collisions()?;
        for action in Action::ALL {
            if matches!(action, Action::Cancel | Action::RenameProject) {
                continue;
            }
            for sequence in &bindings.parsed[&action] {
                if let [stroke] = sequence.as_slice() {
                    bindings.main.insert(stroke.clone(), action);
                }
            }
        }
        for (display, configured_project) in special_add_config {
            let stroke = parse_stroke(&display)
                .with_context(|| format!("invalid special-add binding '{display}'"))?;
            let project = configured_project.trim().trim_start_matches('#').trim();
            if project.is_empty() {
                bail!("special-add binding '{display}' must name a project");
            }
            if bindings.main.contains_key(&stroke)
                || bindings
                    .special_add
                    .iter()
                    .any(|binding| binding.stroke == stroke)
            {
                bail!("special-add binding '{display}' conflicts with another binding");
            }
            bindings.special_add.push(SpecialAddBinding {
                stroke,
                display,
                project: project.to_owned(),
            });
        }
        for (configured_prefix, configured_project) in project_jump_config {
            let prefix = configured_prefix.trim().to_lowercase();
            if prefix.chars().count() < 2 {
                bail!(
                    "project-jump prefix '{configured_prefix}' must contain at least two characters"
                );
            }
            if !prefix.chars().all(|character| {
                character.is_alphanumeric() || matches!(character, ' ' | '-' | '_')
            }) {
                bail!("project-jump prefix '{configured_prefix}' contains unsupported characters");
            }
            let project = configured_project.trim().trim_start_matches('#').trim();
            if project.is_empty() {
                bail!("project-jump prefix '{configured_prefix}' must name a project");
            }
            if bindings.project_jumps.keys().any(|existing| {
                existing.starts_with(&prefix) || prefix.starts_with(existing.as_str())
            }) {
                bail!(
                    "project-jump prefix '{configured_prefix}' conflicts with another project-jump prefix"
                );
            }
            bindings.project_jumps.insert(prefix, project.to_owned());
            bindings.custom_jump_projects.insert(project.to_lowercase());
        }
        Ok(bindings)
    }

    fn validate_first_sequence(&self) -> Result<()> {
        let jumps = &self.parsed[&Action::Jump];
        for sequence in &self.parsed[&Action::First] {
            if sequence.len() == 2
                && !jumps
                    .iter()
                    .any(|jump| jump.len() == 1 && jump[0] == sequence[0])
            {
                bail!("a two-key 'first' binding must begin with a configured 'jump' key");
            }
        }
        Ok(())
    }

    fn validate_main_collisions(&self) -> Result<()> {
        let contextual = [Action::Cancel, Action::CloseDialog];
        let mut seen: Vec<(Action, &KeyStroke)> = Vec::new();
        for action in Action::ALL {
            if contextual.contains(&action) {
                continue;
            }
            for sequence in &self.parsed[&action] {
                if sequence.len() != 1 {
                    continue;
                }
                if let Some((other, _)) = seen.iter().find(|(_, stroke)| **stroke == sequence[0]) {
                    if matches!(
                        (*other, action),
                        (Action::Sync, Action::RenameProject)
                            | (Action::RenameProject, Action::Sync)
                    ) {
                        continue;
                    }
                    bail!(
                        "'{}' and '{}' both use {}",
                        other.name(),
                        action.name(),
                        self.display(action)
                    );
                }
                seen.push((action, &sequence[0]));
            }
        }
        Ok(())
    }
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::DownFive => "down_five",
            Self::UpFive => "up_five",
            Self::FocusNavigation => "focus_navigation",
            Self::FocusTasks => "focus_tasks",
            Self::Confirm => "confirm",
            Self::SearchProjects => "search_projects",
            Self::SearchTasks => "search_tasks",
            Self::SearchFilters => "search_filters",
            Self::AddFilter => "add_filter",
            Self::RenameProject => "rename_project",
            Self::AddProject => "add_project",
            Self::MoveProjectDown => "move_project_down",
            Self::MoveProjectUp => "move_project_up",
            Self::CopyTitle => "copy_title",
            Self::EditTask => "edit_task",
            Self::CompleteTask => "complete_task",
            Self::DeleteTask => "delete_task",
            Self::MoveTask => "move_task",
            Self::AddChildTask => "add_child_task",
            Self::IndentTask => "indent_task",
            Self::OutdentTask => "outdent_task",
            Self::EditDue => "edit_due",
            Self::RemoveDue => "remove_due",
            Self::ShowError => "show_error",
            Self::AddTaskTop => "add_task_top",
            Self::AddTaskBottom => "add_task_bottom",
            Self::Quit => "quit",
            Self::First => "first",
            Self::Last => "last",
            Self::Jump => "jump",
            Self::Cancel => "cancel",
            Self::Sync => "sync",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::ToggleLabelGrouping => "toggle_label_grouping",
            Self::ToggleAlphabeticalSort => "toggle_alphabetical_sort",
            Self::ToggleCompletedTasks => "toggle_completed_tasks",
            Self::ToggleHiddenProjects => "toggle_hidden_projects",
            Self::ToggleProjectHidden => "toggle_project_hidden",
            Self::Space0 => "space_0",
            Self::Space1 => "space_1",
            Self::Space2 => "space_2",
            Self::Space3 => "space_3",
            Self::Space4 => "space_4",
            Self::Space5 => "space_5",
            Self::Space6 => "space_6",
            Self::Space7 => "space_7",
            Self::Space8 => "space_8",
            Self::Space9 => "space_9",
            Self::EditLabels => "edit_labels",
            Self::Help => "help",
            Self::CloseDialog => "close_dialog",
        }
    }

    pub const fn for_space(space: u8) -> Option<Self> {
        match space {
            0 => Some(Self::Space0),
            1 => Some(Self::Space1),
            2 => Some(Self::Space2),
            3 => Some(Self::Space3),
            4 => Some(Self::Space4),
            5 => Some(Self::Space5),
            6 => Some(Self::Space6),
            7 => Some(Self::Space7),
            8 => Some(Self::Space8),
            9 => Some(Self::Space9),
            _ => None,
        }
    }

    pub const fn space(self) -> Option<u8> {
        match self {
            Self::Space0 => Some(0),
            Self::Space1 => Some(1),
            Self::Space2 => Some(2),
            Self::Space3 => Some(3),
            Self::Space4 => Some(4),
            Self::Space5 => Some(5),
            Self::Space6 => Some(6),
            Self::Space7 => Some(7),
            Self::Space8 => Some(8),
            Self::Space9 => Some(9),
            _ => None,
        }
    }
}

impl KeyStroke {
    fn from_event(event: KeyEvent) -> Self {
        let mut modifiers = event.modifiers
            & (KeyModifiers::SHIFT
                | KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER);
        let code = match event.code {
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                modifiers.insert(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_ascii_lowercase())
            }
            KeyCode::Char(c) => {
                if !c.is_alphabetic() {
                    modifiers.remove(KeyModifiers::SHIFT);
                }
                KeyCode::Char(c)
            }
            code => code,
        };
        Self { code, modifiers }
    }
}

fn parse_sequence(value: &str) -> Result<Sequence> {
    let sequence = value
        .split_whitespace()
        .map(parse_stroke)
        .collect::<Result<Vec<_>>>()?;
    if sequence.is_empty() {
        bail!("keybinding cannot be empty");
    }
    Ok(sequence)
}

fn parse_stroke(value: &str) -> Result<KeyStroke> {
    let parts = value.split('+').collect::<Vec<_>>();
    let key = parts.last().copied().unwrap_or_default().trim();
    let mut modifiers = KeyModifiers::NONE;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        match modifier.trim().to_ascii_lowercase().as_str() {
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "super" => modifiers.insert(KeyModifiers::SUPER),
            unknown => bail!("unknown key modifier '{unknown}' in '{value}'"),
        }
    }
    let lower = key.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        _ if lower.starts_with('f') && lower[1..].parse::<u8>().is_ok() => {
            KeyCode::F(lower[1..].parse()?)
        }
        _ => {
            let mut chars = key.chars();
            let Some(mut character) = chars.next() else {
                bail!("missing key in '{value}'");
            };
            if chars.next().is_some() {
                bail!("unknown key '{key}' in '{value}'");
            }
            if character.is_ascii_uppercase() {
                modifiers.insert(KeyModifiers::SHIFT);
                character.make_ascii_lowercase();
            }
            KeyCode::Char(character)
        }
    };
    Ok(KeyStroke { code, modifiers })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_named_keys() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::Quit,
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::Help,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::Down,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn partial_toml_uses_defaults_for_unspecified_bindings() {
        let file: FileConfig = toml::from_str("[keybindings]\ndown = ['x']").unwrap();
        let bindings = KeyBindings::from_config(file.keybindings).unwrap();
        assert!(bindings.matches(
            Action::Down,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::Up,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn visual_line_navigation_defaults_on_and_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let default_path = dir.path().join("default.toml");
        fs::write(&default_path, "[keybindings]\ndown = ['x']\n").unwrap();
        let (_, editor) = KeyBindings::load_with_editor_options(&default_path, true).unwrap();
        assert!(editor.visual_line_navigation);

        let disabled_path = dir.path().join("disabled.toml");
        fs::write(&disabled_path, "[editor]\nvisual_line_navigation = false\n").unwrap();
        let (_, editor) = KeyBindings::load_with_editor_options(&disabled_path, true).unwrap();
        assert!(!editor.visual_line_navigation);
    }

    #[test]
    fn insert_enter_saving_defaults_off_and_can_be_enabled_or_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (_, editor) = KeyBindings::load_with_editor_options(&path, false).unwrap();
        assert!(!editor.save_on_enter_in_insert);
        for (source, expected) in [
            ("[editor]\nvisual_line_navigation = false\n", false),
            ("[editor]\nsave_on_enter_in_insert = true\n", true),
            ("[editor]\nsave_on_enter_in_insert = false\n", false),
        ] {
            fs::write(&path, source).unwrap();
            let (_, editor) = KeyBindings::load_with_editor_options(&path, true).unwrap();
            assert_eq!(editor.save_on_enter_in_insert, expected);
        }
        fs::write(&path, "[editor]\nsave_on_enter_in_insert = 'yes'\n").unwrap();
        assert!(KeyBindings::load_with_editor_options(&path, true).is_err());
    }

    #[test]
    fn recognizes_configured_first_sequence_after_jump() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.follows_jump(
            Action::First,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn move_task_defaults_to_v() {
        assert!(KeyBindings::defaults().matches(
            Action::MoveTask,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn task_search_defaults_to_f() {
        assert!(KeyBindings::defaults().matches(
            Action::SearchTasks,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn project_search_defaults_to_p() {
        assert!(KeyBindings::defaults().matches(
            Action::SearchProjects,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn contextual_reordering_defaults_to_shift_j_and_shift_k() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::MoveProjectDown,
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::MoveProjectUp,
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)
        ));
        assert!(!bindings.matches(
            Action::DownFive,
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)
        ));
        assert!(!bindings.matches(
            Action::UpFive,
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn filter_search_and_add_use_shift_f_and_control_f() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::SearchFilters,
            KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::AddFilter,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn project_rename_shares_r_with_sync_and_add_uses_shift_p() {
        let bindings = KeyBindings::defaults();
        let r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(bindings.matches(Action::RenameProject, r));
        assert!(bindings.matches(Action::Sync, r));
        assert_eq!(bindings.main_action(r), Some(Action::Sync));
        assert!(bindings.matches(
            Action::AddProject,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn copy_title_defaults_to_y() {
        assert!(KeyBindings::defaults().matches(
            Action::CopyTitle,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn task_edit_defaults_to_e_only() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::EditTask,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)
        ));
        assert!(!bindings.matches(
            Action::EditTask,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn hidden_project_bindings_are_distinct() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::ToggleHiddenProjects,
            KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::ToggleProjectHidden,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn task_view_toggles_use_their_defaults() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::ToggleLabelGrouping,
            KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::ToggleAlphabeticalSort,
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)
        ));
        assert!(
            bindings
                .main_action(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
                .is_none()
        );
        assert!(
            bindings
                .main_action(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
                .is_none()
        );
        assert!(bindings.matches(
            Action::ToggleCompletedTasks,
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn delete_remove_due_and_error_details_use_shifted_defaults() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::DeleteTask,
            KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::RemoveDue,
            KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::ShowError,
            KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn task_add_defaults_distinguish_top_and_bottom() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::AddTaskTop,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::AddTaskBottom,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn task_add_placement_bindings_are_independently_configurable() {
        let file: FileConfig =
            toml::from_str("[keybindings]\nadd_task_top = ['x']\nadd_task_bottom = ['z']\n")
                .unwrap();
        let bindings = KeyBindings::from_config(file.keybindings).unwrap();
        assert!(bindings.matches(
            Action::AddTaskTop,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::AddTaskBottom,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)
        ));

        let legacy: FileConfig = toml::from_str("[keybindings]\nadd_task = ['x']\n").unwrap();
        let bindings = KeyBindings::from_config(legacy.keybindings).unwrap();
        assert!(bindings.matches(
            Action::AddTaskBottom,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
        assert!(!bindings.matches(
            Action::AddTaskTop,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn completion_and_undo_use_their_defaults() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::CompleteTask,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::Undo,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::Redo,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)
        ));
        assert!(bindings.matches(
            Action::Sync,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn task_hierarchy_actions_use_c_and_angle_brackets() {
        let bindings = KeyBindings::defaults();
        assert!(bindings.matches(
            Action::AddChildTask,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)
        ));
        assert!(bindings.matches(
            Action::IndentTask,
            KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT)
        ));
        assert!(bindings.matches(
            Action::OutdentTask,
            KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn project_spaces_and_label_binding_use_their_new_defaults() {
        let bindings = KeyBindings::defaults();
        assert_eq!(
            bindings
                .main_action(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL))
                .and_then(Action::space),
            Some(7)
        );
        assert_eq!(
            bindings
                .main_action(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
                .and_then(Action::space),
            None
        );
        assert_eq!(
            bindings
                .main_action(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT))
                .and_then(Action::space),
            None
        );
        assert!(!bindings.matches(
            Action::EditLabels,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)
        ));
        assert!(bindings.matches(
            Action::EditLabels,
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn rejects_conflicting_main_screen_bindings() {
        let file: FileConfig = toml::from_str("[keybindings]\ndown = ['x']\nup = ['x']").unwrap();
        assert!(KeyBindings::from_config(file.keybindings).is_err());
    }

    #[test]
    fn parses_special_add_bindings_and_normalizes_project_names() {
        let file: FileConfig =
            toml::from_str("[special_add]\nw = '#week'\nn = ' Notes '\n").unwrap();
        let bindings = KeyBindings::from_file_config(file).unwrap();
        assert_eq!(
            bindings.special_add_project(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some("week")
        );
        assert_eq!(
            bindings.special_add_project(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some("Notes")
        );
    }

    #[test]
    fn rejects_special_add_collision_with_regular_binding() {
        let file: FileConfig = toml::from_str("[special_add]\nu = '#week'\n").unwrap();
        assert!(KeyBindings::from_file_config(file).is_err());
    }

    #[test]
    fn special_add_has_no_default_mapping() {
        assert_eq!(KeyBindings::defaults().special_add_bindings().count(), 0);
    }

    #[test]
    fn parses_project_jump_prefixes_and_normalizes_projects() {
        let file: FileConfig =
            toml::from_str("[project_jumps]\nWW = '#week'\nno = ' Notes '\n").unwrap();
        let bindings = KeyBindings::from_file_config(file).unwrap();

        assert_eq!(bindings.project_jump_target("ww"), Some("week"));
        assert_eq!(bindings.project_jump_target("no"), Some("Notes"));
        assert!(bindings.has_project_jump_with_prefix("w"));
        assert!(!bindings.has_project_jump_with_prefix("x"));
        assert!(bindings.project_uses_custom_jump("WEEK"));
        assert!(!bindings.project_uses_custom_jump("Weekend"));
    }

    #[test]
    fn rejects_short_or_overlapping_project_jump_prefixes() {
        let short: FileConfig = toml::from_str("[project_jumps]\nw = '#week'\n").unwrap();
        assert!(KeyBindings::from_file_config(short).is_err());

        let overlapping: FileConfig =
            toml::from_str("[project_jumps]\nww = '#week'\nwww = '#work'\n").unwrap();
        assert!(KeyBindings::from_file_config(overlapping).is_err());
    }

    #[test]
    fn project_jumps_have_no_default_mapping() {
        assert_eq!(KeyBindings::defaults().project_jump_bindings().count(), 0);
    }

    #[test]
    fn distributed_example_is_valid() {
        let file: FileConfig = toml::from_str(include_str!("../config.example.toml")).unwrap();
        KeyBindings::from_file_config(file).unwrap();
    }
}

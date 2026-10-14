mod app;
mod clipboard;
mod config;
mod editor;
mod keymap;
mod model;
mod store;
mod sync;
mod todoist;
mod ui;

use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use app::App;
use config::Config;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use store::Store;
use sync::SyncManager;

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            SetCursorStyle::DefaultUserShape,
            crossterm::cursor::Show
        );
    }
}

fn main() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("todoist-tui must run in an interactive terminal");
    }
    let root = application_root()?;
    let config = Config::load(&root)?;
    let store = Store::open(&config.database_path)?;
    let sync = SyncManager::new(store.clone(), config.token)?;
    let mut app = App::new(store, sync, config.keybindings, config.config_path)?;
    app.set_editor_visual_line_navigation(config.visual_line_navigation);

    enable_raw_mode()?;
    // Install the guard immediately after raw mode succeeds so every later
    // startup error restores the user's terminal.
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;
    // Request unambiguous modified-key reporting where the terminal supports it.
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    );
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut redraw = true;
    while !app.quit {
        if redraw {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            if let Some(shape) = app.editor_cursor_shape() {
                let style = match shape {
                    editor::EditorCursorShape::Block => SetCursorStyle::SteadyBlock,
                    editor::EditorCursorShape::Bar => SetCursorStyle::SteadyBar,
                };
                execute!(terminal.backend_mut(), style)?;
            }
            redraw = false;
        }
        if event::poll(app.poll_timeout())? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    app.handle_key(key)?;
                    redraw = true;
                }
                Event::Resize(_, _) => {
                    terminal.autoresize()?;
                    redraw = true;
                }
                _ => {}
            }
        }
        redraw |= app.tick()?;
    }
    Ok(())
}

fn application_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let exe = std::env::current_exe().context("determine executable path")?;
    Ok(find_application_root(&cwd, &exe))
}

fn find_application_root(cwd: &Path, executable: &Path) -> PathBuf {
    if is_application_root(cwd) {
        return cwd.to_path_buf();
    }
    executable
        .parent()
        .and_then(|parent| parent.ancestors().find(|path| is_application_root(path)))
        .unwrap_or_else(|| executable.parent().unwrap_or(cwd))
        .to_path_buf()
}

fn is_application_root(path: &Path) -> bool {
    path.join("API_TOKEN").is_file() || path.join("Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repository_root_above_release_binary() {
        let dir = tempfile::tempdir().unwrap();
        let repository = dir.path().join("todoist-tui");
        let executable = repository.join("target/release/todoist-tui");
        let unrelated_cwd = dir.path().join("scratchpad");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&unrelated_cwd).unwrap();
        std::fs::write(repository.join("Cargo.toml"), "").unwrap();
        std::fs::write(repository.join("API_TOKEN"), "not-a-real-token").unwrap();

        assert_eq!(
            find_application_root(&unrelated_cwd, &executable),
            repository
        );
    }
}

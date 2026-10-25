use std::{
    fs,
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorOptions {
    pub visual_line_navigation: bool,
    pub save_on_enter_in_insert: bool,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self {
            visual_line_navigation: true,
            save_on_enter_in_insert: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPurpose {
    Add,
    AddChild,
    Edit,
    Due,
    AddFilter,
    EditFilter,
    AddProject,
    RenameProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCursorShape {
    Block = 2,
    Bar = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorCellStyle {
    pub foreground: vt100::Color,
    pub background: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSpan {
    pub text: String,
    pub style: EditorCellStyle,
}

const DOUBLE_ESCAPE_INTERVAL: Duration = Duration::from_millis(400);
const FALLBACK_VISUAL_DOWN: &str = "lua if vim.fn.maparg('j', 'n') == '' then vim.keymap.set('n', 'j', 'gj', { buffer = true, silent = true, noremap = true }) end";
const FALLBACK_VISUAL_UP: &str = "lua if vim.fn.maparg('k', 'n') == '' then vim.keymap.set('n', 'k', 'gk', { buffer = true, silent = true, noremap = true }) end";
const ENTER_MAPPINGS: &str = "lua vim.keymap.set('i', '<S-CR>', '<CR>', { buffer = true, silent = true }); vim.keymap.set('n', '<CR>', '<Cmd>write<CR>', { buffer = true, silent = true })";

pub struct EmbeddedEditor {
    purpose: EditorPurpose,
    task_id: Option<String>,
    dir: TempDir,
    parser: Arc<Mutex<vt100::Parser>>,
    dirty: Arc<AtomicBool>,
    cursor_shape: Arc<AtomicU8>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    last_escape: Option<Instant>,
    rows: u16,
    cols: u16,
    options: EditorOptions,
    notice: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditorResult {
    pub purpose: EditorPurpose,
    pub task_id: Option<String>,
    pub accepted: bool,
    pub text: String,
}

impl EmbeddedEditor {
    #[cfg(test)]
    pub fn purpose(&self) -> EditorPurpose {
        self.purpose
    }

    pub fn edits_filter(&self) -> bool {
        matches!(
            self.purpose,
            EditorPurpose::AddFilter | EditorPurpose::EditFilter
        )
    }

    pub fn save_on_enter_in_insert(&self) -> bool {
        self.options.save_on_enter_in_insert
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, message: String) {
        self.notice = Some(message);
    }

    pub fn open(
        purpose: EditorPurpose,
        task_id: Option<String>,
        initial: &str,
        rows: u16,
        cols: u16,
        options: EditorOptions,
    ) -> Result<Self> {
        Self::open_inner_with_options(purpose, task_id, initial, rows, cols, true, options)
    }

    #[cfg(test)]
    fn open_inner(
        purpose: EditorPurpose,
        task_id: Option<String>,
        initial: &str,
        rows: u16,
        cols: u16,
        load_user_config: bool,
    ) -> Result<Self> {
        Self::open_inner_with_options(
            purpose,
            task_id,
            initial,
            rows,
            cols,
            load_user_config,
            EditorOptions::default(),
        )
    }

    fn open_inner_with_options(
        purpose: EditorPurpose,
        task_id: Option<String>,
        initial: &str,
        rows: u16,
        cols: u16,
        load_user_config: bool,
        options: EditorOptions,
    ) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("todoist-tui-editor-")
            .tempdir()?;
        let file = dir.path().join("value.txt");
        let marker = dir.path().join("saved");
        fs::write(&file, initial)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(3),
            cols: cols.max(10),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new("nvim");
        command.env("NVIM_LOG_FILE", dir.path().join("nvim.log"));
        // The embedded renderer understands standard xterm control sequences,
        // not terminal-specific protocols inherited from the outer terminal.
        command.env("TERM", "xterm-256color");
        command.arg("-n");
        if !load_user_config {
            command.args(["-u", "NONE", "-i", "NONE"]);
        }
        command.args([
            "-c",
            "set noswapfile wrap linebreak laststatus=0 noshowmode noruler",
            "-c",
            "set shortmess+=I",
            "-c",
            "set guicursor=n-v-c-sm:block-blinkon0,i-ci-ve:ver25-blinkon0,r-cr-o:hor20-blinkon0",
        ]);
        if options.visual_line_navigation {
            command.args(["-c", FALLBACK_VISUAL_DOWN, "-c", FALLBACK_VISUAL_UP]);
        }
        match purpose {
            EditorPurpose::Add
            | EditorPurpose::AddChild
            | EditorPurpose::AddFilter
            | EditorPurpose::AddProject => {
                command.args(["-c", "startinsert"]);
            }
            EditorPurpose::Due => {
                command.args(["-c", "normal! G$", "-c", "startinsert!"]);
            }
            EditorPurpose::Edit | EditorPurpose::EditFilter | EditorPurpose::RenameProject => {
                command.args(["-c", "normal! gg$", "-c", "startinsert!"]);
            }
        }
        if options.save_on_enter_in_insert {
            command.args(["-c", "inoremap <silent> <buffer> <CR> <Esc>:write<CR>"]);
        }
        command.args(["-c", ENTER_MAPPINGS]);
        let autocmd = format!(
            "autocmd BufWritePost <buffer> call writefile(['ok'], '{}') | silent! qall!",
            marker.to_string_lossy().replace('\'', "''")
        );
        command.args(["-c", &autocmd]);
        command.arg(file.as_os_str());
        let mut child = pair.slave.spawn_command(command).context("start Neovim")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows.max(3), cols.max(10), 0)));
        let dirty = Arc::new(AtomicBool::new(true));
        let cursor_shape = Arc::new(AtomicU8::new(EditorCursorShape::Bar as u8));
        let output = parser.clone();
        let output_dirty = dirty.clone();
        let output_cursor_shape = cursor_shape.clone();
        let terminal_writer = writer.clone();
        let reader_thread = thread::Builder::new()
            .name("todoist-tui-nvim-output".into())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                let mut query_buffer = [0_u8; 8200];
                let mut tail_len = 0;
                while let Ok(count) = reader.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    query_buffer[tail_len..tail_len + count].copy_from_slice(&buffer[..count]);
                    let response_len = tail_len + count;
                    let response_input = &query_buffer[..response_len];
                    respond_to_terminal_queries(response_input, &terminal_writer);
                    update_cursor_shape(response_input, &output_cursor_shape);
                    tail_len = response_len.min(8);
                    query_buffer.copy_within(response_len - tail_len..response_len, 0);
                    if let Ok(mut parser) = output.lock() {
                        parser.process(&buffer[..count]);
                        output_dirty.store(true, Ordering::Release);
                    }
                }
            });
        if let Err(error) = reader_thread {
            let _ = child.kill();
            return Err(error).context("start embedded Neovim output reader");
        }
        Ok(Self {
            purpose,
            task_id,
            dir,
            parser,
            dirty,
            cursor_shape,
            writer,
            master: pair.master,
            child,
            last_escape: None,
            rows: rows.max(3),
            cols: cols.max(10),
            options,
            notice: None,
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(3);
        let cols = cols.max(10);
        if rows == self.rows && cols == self.cols {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
    }

    pub fn send_key(&mut self, key: KeyEvent) -> Result<bool> {
        if rapid_double_escape(&mut self.last_escape, key, Instant::now()) {
            let _ = self.child.kill();
            return Ok(true);
        }
        let bytes = key_bytes(key);
        if !bytes.is_empty() {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("embedded Neovim input lock was poisoned"))?;
            writer.write_all(&bytes)?;
            writer.flush()?;
        }
        Ok(false)
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.parser
            .lock()
            .map(|parser| visible_screen_lines(parser.screen()))
            .unwrap_or_default()
    }

    pub fn styled_lines(&self) -> Vec<Vec<EditorSpan>> {
        self.parser
            .lock()
            .map(|parser| visible_screen_styled_lines(parser.screen()))
            .unwrap_or_default()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.parser
            .lock()
            .map(|p| p.screen().cursor_position())
            .unwrap_or((0, 0))
    }

    pub fn cursor_shape(&self) -> EditorCursorShape {
        match self.cursor_shape.load(Ordering::Acquire) {
            value if value == EditorCursorShape::Block as u8 => EditorCursorShape::Block,
            _ => EditorCursorShape::Bar,
        }
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn poll(&mut self) -> Result<Option<EditorResult>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        let accepted = status.success() && self.dir.path().join("saved").exists();
        let text = fs::read_to_string(self.dir.path().join("value.txt")).unwrap_or_default();
        Ok(Some(EditorResult {
            purpose: self.purpose,
            task_id: self.task_id.clone(),
            accepted,
            text,
        }))
    }
}

#[cfg(test)]
fn visible_screen_lines(screen: &vt100::Screen) -> Vec<String> {
    let (_, columns) = screen.size();
    // `Screen::contents()` joins terminal rows that were created by soft
    // wrapping. The TUI renders physical PTY rows, so joining them produces a
    // line wider than the modal: Ratatui clips its continuation while Neovim's
    // cursor still advances. Preserve every visible terminal row instead.
    screen.rows(0, columns).collect()
}

fn visible_screen_styled_lines(screen: &vt100::Screen) -> Vec<Vec<EditorSpan>> {
    let (rows, columns) = screen.size();
    (0..rows)
        .map(|row| {
            let mut spans: Vec<EditorSpan> = Vec::new();
            for column in 0..columns {
                let Some(cell) = screen.cell(row, column) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let style = EditorCellStyle {
                    foreground: cell.fgcolor(),
                    background: cell.bgcolor(),
                    bold: cell.bold(),
                    dim: cell.dim(),
                    italic: cell.italic(),
                    underline: cell.underline(),
                    inverse: cell.inverse(),
                };
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                if let Some(previous) = spans.last_mut().filter(|span| span.style == style) {
                    previous.text.push_str(text);
                } else {
                    spans.push(EditorSpan {
                        text: text.to_owned(),
                        style,
                    });
                }
            }
            while let Some(last) = spans.last_mut() {
                if last.style != default_editor_cell_style() {
                    break;
                }
                let trimmed = last.text.trim_end_matches(' ').len();
                last.text.truncate(trimmed);
                if last.text.is_empty() {
                    spans.pop();
                } else {
                    break;
                }
            }
            spans
        })
        .collect()
}

fn default_editor_cell_style() -> EditorCellStyle {
    EditorCellStyle {
        foreground: vt100::Color::Default,
        background: vt100::Color::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
    }
}

impl Drop for EmbeddedEditor {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn rapid_double_escape(last_escape: &mut Option<Instant>, key: KeyEvent, now: Instant) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.code != KeyCode::Esc {
        *last_escape = None;
        return false;
    }
    if last_escape
        .take()
        .is_some_and(|previous| now.saturating_duration_since(previous) <= DOUBLE_ESCAPE_INTERVAL)
    {
        return true;
    }
    *last_escape = Some(now);
    false
}

fn respond_to_terminal_queries(output: &[u8], writer: &Arc<Mutex<Box<dyn Write + Send>>>) {
    let response = if output.windows(4).any(|bytes| bytes == b"\x1b[5n") {
        Some(b"\x1b[0n".as_slice())
    } else if output.windows(4).any(|bytes| bytes == b"\x1b[6n") {
        Some(b"\x1b[1;1R".as_slice())
    } else {
        None
    };
    if let (Some(response), Ok(mut writer)) = (response, writer.lock()) {
        let _ = writer.write_all(response);
        let _ = writer.flush();
    }
}

fn update_cursor_shape(output: &[u8], cursor_shape: &AtomicU8) {
    for sequence in output.windows(5) {
        let shape = match sequence {
            b"\x1b[1 q" | b"\x1b[2 q" => Some(EditorCursorShape::Block),
            b"\x1b[5 q" | b"\x1b[6 q" => Some(EditorCursorShape::Bar),
            _ => None,
        };
        if let Some(shape) = shape {
            cursor_shape.store(shape as u8, Ordering::Release);
        }
    }
}

fn character_bytes(character: char, shifted: bool) -> Vec<u8> {
    if shifted && character.is_lowercase() {
        let mut uppercase = character.to_uppercase();
        let first = uppercase.next().unwrap_or(character);
        if uppercase.next().is_none() {
            first.to_string().into_bytes()
        } else {
            character.to_string().into_bytes()
        }
    } else {
        character.to_string().into_bytes()
    }
}

fn key_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let control = match c.to_ascii_lowercase() {
                'a'..='z' => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
                ' ' | '@' => Some(0),
                '[' => Some(27),
                '\\' => Some(28),
                ']' => Some(29),
                '^' => Some(30),
                '_' => Some(31),
                '?' => Some(127),
                _ => None,
            };
            if let Some(control) = control {
                let mut bytes = Vec::new();
                if key.modifiers.contains(KeyModifiers::ALT) {
                    bytes.push(0x1b);
                }
                bytes.push(control);
                bytes
            } else {
                // Do not truncate Unicode to u8 and accidentally generate an
                // unrelated ASCII control key on a non-US keyboard.
                let parameter = 1
                    + u8::from(key.modifiers.contains(KeyModifiers::SHIFT))
                    + 2 * u8::from(key.modifiers.contains(KeyModifiers::ALT))
                    + 4
                    + 8 * u8::from(key.modifiers.contains(KeyModifiers::SUPER));
                format!("\x1b[{};{parameter}u", c as u32).into_bytes()
            }
        }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
            let mut bytes = vec![0x1b];
            bytes.extend(character_bytes(
                c,
                key.modifiers.contains(KeyModifiers::SHIFT),
            ));
            bytes
        }
        KeyCode::Char(c) => character_bytes(c, key.modifiers.contains(KeyModifiers::SHIFT)),
        // CSI-u keeps Shift+Enter distinct from CR inside the Neovim PTY.
        // Its nonrecursive Insert-mode mapping bypasses save-on-Enter.
        KeyCode::Enter if key.modifiers == KeyModifiers::SHIFT => b"\x1b[13;2u".to_vec(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(number) => function_key_bytes(number, key.modifiers),
        _ => Vec::new(),
    }
}

fn function_key_bytes(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    // xterm defines F13–F63 as modified F1–F12. Larger numbers
    // have no standard xterm encoding; never wrap u8 arithmetic or emit text.
    let (base, implicit) = match number {
        1..=12 => (number, KeyModifiers::NONE),
        13..=24 => (number - 12, KeyModifiers::SHIFT),
        25..=36 => (number - 24, KeyModifiers::CONTROL),
        37..=48 => (number - 36, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
        49..=60 => (number - 48, KeyModifiers::ALT),
        61..=63 => (number - 60, KeyModifiers::ALT | KeyModifiers::SHIFT),
        _ => return Vec::new(),
    };
    let modifiers = modifiers | implicit;
    let parameter = 1
        + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::SUPER));
    if base <= 4 {
        let letter = b"PQRS"[(base - 1) as usize] as char;
        if parameter == 1 {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[1;{parameter}{letter}").into_bytes()
        }
    } else {
        let code = [15, 17, 18, 19, 20, 21, 23, 24][(base - 5) as usize];
        if parameter == 1 {
            format!("\x1b[{code}~").into_bytes()
        } else {
            format!("\x1b[{code};{parameter}~").into_bytes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn visible_lines_preserve_soft_wrapped_terminal_rows() {
        let mut parser = vt100::Parser::new(3, 5, 0);
        parser.process(b"abcdef");

        let lines = visible_screen_lines(parser.screen());

        assert_eq!(lines[0], "abcde");
        assert_eq!(lines[1], "f");
    }

    #[test]
    fn styled_lines_preserve_terminal_selection_backgrounds() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"ab\x1b[48;5;8mcd\x1b[0m");

        let lines = visible_screen_styled_lines(parser.screen());

        assert_eq!(lines[0][0].text, "ab");
        assert_eq!(lines[0][0].style.background, vt100::Color::Default);
        assert_eq!(lines[0][1].text, "cd");
        assert_eq!(lines[0][1].style.background, vt100::Color::Idx(8));
    }

    #[test]
    fn shifted_letters_are_forwarded_as_uppercase() {
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT)),
            b"A"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            b"A"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            )),
            b"\x1bA"
        );
    }

    #[test]
    fn layout_resolved_characters_are_forwarded_unchanged() {
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)),
            b":"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('ä'), KeyModifiers::NONE)),
            "ä".as_bytes()
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('ß'), KeyModifiers::SHIFT)),
            "ß".as_bytes()
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('š'), KeyModifiers::CONTROL)),
            b"\x1b[353;5u"
        );
    }

    #[test]
    fn function_keys_use_standard_sequences_and_high_numbers_never_overflow() {
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            b"\x1bOP"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            b"\x1b[15~"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::F(12), KeyModifiers::CONTROL)),
            b"\x1b[24;5~"
        );
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::F(13), KeyModifiers::NONE)),
            b"\x1b[1;2P"
        );
        for number in [0, 64, 245, 255] {
            assert!(key_bytes(KeyEvent::new(KeyCode::F(number), KeyModifiers::NONE)).is_empty());
        }
    }

    #[test]
    fn forwarded_function_keys_trigger_their_neovim_mappings() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "", 8, 70, false).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        let command = ":lua for _, n in ipairs({1, 4, 5, 12, 13, 24, 36}) do vim.keymap.set(\"i\", \"<F\" .. n .. \">\", \"[\" .. n .. \"]\", {buffer=true}) end";
        for character in command.chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
            .unwrap();
        for number in [1, 4, 5, 12, 13, 24, 36] {
            editor
                .send_key(KeyEvent::new(KeyCode::F(number), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "[1][4][5][12][13][24][36]");
    }

    #[test]
    fn only_rapid_consecutive_escape_presses_cancel() {
        let start = Instant::now();
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut last = None;

        assert!(!rapid_double_escape(&mut last, escape, start));
        assert!(!rapid_double_escape(
            &mut last,
            escape,
            start + DOUBLE_ESCAPE_INTERVAL + Duration::from_millis(1)
        ));
        assert!(rapid_double_escape(
            &mut last,
            escape,
            start + DOUBLE_ESCAPE_INTERVAL + Duration::from_millis(100)
        ));

        last = None;
        assert!(!rapid_double_escape(&mut last, escape, start));
        assert!(!rapid_double_escape(
            &mut last,
            other,
            start + Duration::from_millis(20)
        ));
        assert!(!rapid_double_escape(
            &mut last,
            escape,
            start + Duration::from_millis(40)
        ));
    }

    #[test]
    fn neovim_cursor_sequences_select_steady_block_and_bar() {
        let shape = AtomicU8::new(EditorCursorShape::Bar as u8);
        update_cursor_shape(b"prefix\x1b[2 q", &shape);
        assert_eq!(
            shape.load(Ordering::Acquire),
            EditorCursorShape::Block as u8
        );
        update_cursor_shape(b"\x1b[2 qtext\x1b[6 q", &shape);
        assert_eq!(shape.load(Ordering::Acquire), EditorCursorShape::Bar as u8);
    }

    #[test]
    fn visual_line_fallback_preserves_an_init_lua_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        let result = dir.path().join("mappings.txt");
        fs::write(&init, "vim.keymap.set('n', 'j', 'zz')\n").unwrap();
        let record = format!(
            "lua vim.fn.writefile({{vim.fn.maparg('j', 'n'), vim.fn.maparg('k', 'n')}}, [[{}]])",
            result.display()
        );
        let status = std::process::Command::new("nvim")
            .arg("--headless")
            .arg("-u")
            .arg(&init)
            .args(["-i", "NONE", "-c", FALLBACK_VISUAL_DOWN])
            .args(["-c", FALLBACK_VISUAL_UP, "-c", &record, "-c", "qall!"])
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(fs::read_to_string(result).unwrap(), "zz\ngk\n");
    }

    #[test]
    fn embedded_neovim_reports_normal_and_insert_cursor_shapes() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "", 8, 50, false).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        wait_for_cursor_shape(&editor, EditorCursorShape::Block);

        editor
            .send_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
            .unwrap();
        wait_for_cursor_shape(&editor, EditorCursorShape::Bar);

        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        assert!(!wait_for_result(&mut editor).accepted);
    }

    #[test]
    fn rapid_double_escape_cancels_embedded_neovim() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Edit, None, "unchanged", 8, 50, false)
                .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !editor
                .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap()
        );
        assert!(
            editor
                .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap()
        );
        assert!(!wait_for_result(&mut editor).accepted);
    }

    #[test]
    fn writing_in_embedded_neovim_accepts_the_value() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "", 8, 50, false).unwrap();
        for key in [
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = editor.poll().unwrap() {
                assert!(result.accepted);
                assert_eq!(result.text.trim(), "milk");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Neovim did not exit after writing"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn configured_enter_in_insert_mode_accepts_a_new_task() {
        let mut editor = EmbeddedEditor::open_inner_with_options(
            EditorPurpose::Add,
            None,
            "",
            8,
            50,
            false,
            EditorOptions {
                save_on_enter_in_insert: true,
                ..EditorOptions::default()
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for key in [
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = editor.poll().unwrap() {
                assert!(result.accepted);
                assert_eq!(result.text.trim(), "milk");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Neovim did not save and exit after Enter; screen: {:?}",
                (
                    editor.lines(),
                    fs::read_to_string(editor.dir.path().join("value.txt"))
                )
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn insert_enter_inserts_a_newline_by_default() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "", 8, 50, false).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for character in "Title".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        for character in "Description".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        wait_for_description_line(&mut editor);
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text, "Title\nDescription\n");
    }

    #[test]
    fn insert_shift_enter_always_inserts_a_newline_without_saving() {
        assert_ne!(
            key_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            key_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        for save_on_enter_in_insert in [false, true] {
            let mut editor = EmbeddedEditor::open_inner_with_options(
                EditorPurpose::Add,
                None,
                "",
                8,
                50,
                false,
                EditorOptions {
                    save_on_enter_in_insert,
                    ..EditorOptions::default()
                },
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(200));
            for character in "Title".chars() {
                editor
                    .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                    .unwrap();
            }
            editor
                .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
                .unwrap();
            for character in "Description".chars() {
                editor
                    .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                    .unwrap();
            }
            wait_for_description_line(&mut editor);
            editor
                .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .unwrap();
            editor
                .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            let result = wait_for_result(&mut editor);
            assert!(result.accepted);
            assert_eq!(result.text, "Title\nDescription\n");
        }
    }

    #[test]
    fn normal_enter_saves_every_editor_regardless_of_insert_enter_setting() {
        for save_on_enter_in_insert in [false, true] {
            for purpose in [
                EditorPurpose::Add,
                EditorPurpose::AddChild,
                EditorPurpose::Edit,
                EditorPurpose::Due,
                EditorPurpose::AddFilter,
                EditorPurpose::EditFilter,
                EditorPurpose::AddProject,
                EditorPurpose::RenameProject,
            ] {
                let mut editor = EmbeddedEditor::open_inner_with_options(
                    purpose,
                    None,
                    "Content",
                    8,
                    50,
                    false,
                    EditorOptions {
                        save_on_enter_in_insert,
                        ..EditorOptions::default()
                    },
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(200));
                editor
                    .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                    .unwrap();
                editor
                    .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                    .unwrap();
                let result = wait_for_result(&mut editor);
                assert!(
                    result.accepted,
                    "{purpose:?}, save_on_enter_in_insert={save_on_enter_in_insert}"
                );
                assert_eq!(result.text, "Content\n");
            }
        }
    }

    fn wait_for_description_line(editor: &mut EmbeddedEditor) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                editor.poll().unwrap().is_none(),
                "Insert-mode newline unexpectedly closed Neovim"
            );
            if editor.cursor() == (1, 11)
                && editor
                    .lines()
                    .get(1)
                    .is_some_and(|line| line.contains("Description"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Newline was not inserted; screen: {:?}",
                editor.lines()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!editor.dir.path().join("saved").exists());
    }

    #[test]
    fn enter_in_normal_mode_accepts_a_new_task() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "milk", 8, 50, false).unwrap();
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
                assert!(result.accepted);
                assert_eq!(result.text.trim(), "milk");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Neovim did not save and exit after Normal-mode Enter"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn task_edit_starts_in_insert_mode_at_end_of_title() {
        let mut editor = EmbeddedEditor::open_inner(
            EditorPurpose::Edit,
            Some("task-1".into()),
            "milk\ndescription",
            8,
            50,
            false,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for key in [
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "milk!\ndescription");
    }

    #[test]
    fn task_edit_can_fill_the_precreated_description_line() {
        let mut editor = EmbeddedEditor::open_inner(
            EditorPurpose::Edit,
            Some("task-1".into()),
            "milk\n\n",
            8,
            50,
            false,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        editor
            .send_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        for character in "two bottles".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "milk\ntwo bottles");
    }

    #[test]
    fn task_add_can_fill_the_precreated_description_line() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "\n\n", 8, 50, false).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for character in "milk".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        for character in "two bottles".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();

        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "milk\ntwo bottles");
    }

    #[test]
    fn enter_in_normal_mode_accepts_a_task_edit() {
        let mut editor = EmbeddedEditor::open_inner(
            EditorPurpose::Edit,
            Some("task-1".into()),
            "milk",
            8,
            50,
            false,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "mil");
    }

    #[test]
    fn due_date_edit_starts_in_insert_mode() {
        let mut editor = EmbeddedEditor::open_inner(
            EditorPurpose::Due,
            Some("task-1".into()),
            "tom",
            8,
            50,
            false,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        for character in "orrow".chars() {
            editor
                .send_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "tomorrow");
    }

    #[test]
    fn enter_in_normal_mode_accepts_a_due_date() {
        let mut editor = EmbeddedEditor::open_inner(
            EditorPurpose::Due,
            Some("task-1".into()),
            "tod",
            8,
            50,
            false,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        editor
            .send_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        editor
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "tod");
    }

    fn wait_for_result(editor: &mut EmbeddedEditor) -> EditorResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = editor.poll().unwrap() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "Neovim did not save and exit; screen: {:?}",
                editor.lines()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_cursor_shape(editor: &EmbeddedEditor, expected: EditorCursorShape) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while editor.cursor_shape() != expected {
            assert!(
                Instant::now() < deadline,
                "Neovim did not switch to the expected {expected:?} cursor"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

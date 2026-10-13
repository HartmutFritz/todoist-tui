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
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPurpose {
    Add,
    Edit,
    Due,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCursorShape {
    Block = 2,
    Bar = 6,
}

const DOUBLE_ESCAPE_INTERVAL: Duration = Duration::from_millis(400);

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
}

#[derive(Debug)]
pub struct EditorResult {
    pub purpose: EditorPurpose,
    pub task_id: Option<String>,
    pub accepted: bool,
    pub text: String,
}

impl EmbeddedEditor {
    pub fn open(
        purpose: EditorPurpose,
        task_id: Option<String>,
        initial: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::open_inner(purpose, task_id, initial, rows, cols, true)
    }

    fn open_inner(
        purpose: EditorPurpose,
        task_id: Option<String>,
        initial: &str,
        rows: u16,
        cols: u16,
        load_user_config: bool,
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
            "set noswapfile nowrap laststatus=0 noshowmode noruler",
            "-c",
            "set shortmess+=I",
            "-c",
            "set guicursor=n-v-c-sm:block-blinkon0,i-ci-ve:ver25-blinkon0,r-cr-o:hor20-blinkon0",
        ]);
        match purpose {
            EditorPurpose::Add => {
                command.args(["-c", "startinsert"]);
            }
            EditorPurpose::Due => {
                command.args(["-c", "normal! G$", "-c", "startinsert!"]);
            }
            EditorPurpose::Edit => {
                command.args(["-c", "normal! gg$", "-c", "startinsert!"]);
            }
        }
        command.args([
            "-c",
            "inoremap <silent> <buffer> <CR> <Esc>:write<CR>",
            "-c",
            "nnoremap <silent> <buffer> <CR> <Cmd>write<CR>",
        ]);
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

    pub fn lines(&self) -> Vec<String> {
        self.parser
            .lock()
            .map(|p| p.screen().contents().lines().map(str::to_owned).collect())
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
        character.to_uppercase().collect::<String>().into_bytes()
    } else {
        character.to_string().into_bytes()
    }
}

fn key_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = c.to_ascii_lowercase() as u8;
            if lower.is_ascii_lowercase() {
                vec![lower - b'a' + 1]
            } else {
                Vec::new()
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
        KeyCode::F(number) => format!("\x1b[{}~", number + 10).into_bytes(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
    fn enter_in_insert_mode_accepts_a_new_task() {
        let mut editor =
            EmbeddedEditor::open_inner(EditorPurpose::Add, None, "", 8, 50, false).unwrap();
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
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            editor.send_key(key).unwrap();
        }
        let result = wait_for_result(&mut editor);
        assert!(result.accepted);
        assert_eq!(result.text.trim(), "milk!\ndescription");
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

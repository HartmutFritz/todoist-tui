use std::{
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail};

const HELPER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Clipboard {
    job: Option<CopyJob>,
}

struct CopyJob {
    result: Receiver<Result<()>>,
    cancel: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl Clipboard {
    pub fn is_running(&self) -> bool {
        self.job.is_some()
    }

    pub fn start(&mut self, text: String) -> Result<bool> {
        self.start_with(text, |text, cancel| {
            copy_with_commands(clipboard_commands(), &text, HELPER_TIMEOUT, &cancel)
        })
    }

    pub(crate) fn start_with(
        &mut self,
        text: String,
        copy: impl FnOnce(String, Arc<AtomicBool>) -> Result<()> + Send + 'static,
    ) -> Result<bool> {
        if self.job.is_some() {
            return Ok(false);
        }
        let (tx, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let worker = thread::Builder::new()
            .name("todoist-tui-clipboard".into())
            .spawn(move || {
                let _ = tx.send(copy(text, cancelled));
            })
            .context("start clipboard worker")?;
        self.job = Some(CopyJob {
            result,
            cancel,
            worker,
        });
        Ok(true)
    }

    pub fn poll(&mut self) -> Option<Result<()>> {
        let job = self.job.as_ref()?;
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("Clipboard worker stopped unexpectedly"))
            }
        };
        let job = self.job.take().expect("clipboard job exists");
        let _ = job.worker.join();
        Some(result)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Release);
            // The worker checks cancellation during both input and exit waits.
            // Reap helpers before the application process exits.
            let _ = job.worker.join();
        }
    }
}

fn clipboard_commands() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
}

fn copy_with_commands(
    commands: &[(&str, &[&str])],
    text: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut timed_out = false;
    for (program, arguments) in commands {
        if cancel.load(Ordering::Acquire) {
            bail!("Clipboard copy cancelled");
        }
        match copy_with_command(program, arguments, text, timeout, cancel) {
            Ok(true) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::TimedOut => timed_out = true,
            _ => {}
        }
    }
    if timed_out {
        bail!("Clipboard timed out: check your display session");
    }
    bail!(
        "Clipboard unavailable: check your display session or install wl-clipboard, xclip, or xsel"
    )
}

#[cfg(unix)]
use platform::copy_with_command;

#[cfg(not(unix))]
fn copy_with_command(
    _: &str,
    _: &[&str],
    _: &str,
    _: Duration,
    _: &AtomicBool,
) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "clipboard helpers require Unix",
    ))
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        io::{self, Write},
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, Command, Stdio},
        time::Instant,
    };

    struct Helper {
        child: Child,
        successful: bool,
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            if !self.successful {
                // SAFETY: process_group(0) gives this child its own process group;
                // the negative ID targets only that helper and its descendants.
                unsafe {
                    libc::kill(-(self.child.id() as i32), libc::SIGKILL);
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    fn check_deadline(deadline: Instant, cancel: &AtomicBool) -> io::Result<()> {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "clipboard copy cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "clipboard helper timed out",
            ));
        }
        Ok(())
    }

    pub(super) fn copy_with_command(
        program: &str,
        arguments: &[&str],
        text: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        let child = Command::new(program)
            .args(arguments)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut helper = Helper {
            child,
            successful: false,
        };
        let mut stdin = helper.child.stdin.take().expect("piped clipboard input");
        let fd = stdin.as_raw_fd();
        // SAFETY: stdin owns this valid descriptor for both fcntl calls. Only
        // this worker uses the pipe, so making its write end non-blocking is local.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            check_deadline(deadline, cancel)?;
            match stdin.write(remaining) {
                Ok(0) => {
                    return Err(io::Error::new(
                        ErrorKind::WriteZero,
                        "clipboard input closed",
                    ));
                }
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        drop(stdin);
        loop {
            check_deadline(deadline, cancel)?;
            if let Some(status) = helper.child.try_wait()? {
                helper.successful = status.success();
                // Successful wl-copy/xclip helpers may deliberately fork a
                // clipboard owner. Preserve it after the parent exits normally.
                return Ok(helper.successful);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn supports_wayland_and_both_common_x11_helpers() {
        assert_eq!(
            clipboard_commands(),
            &[
                ("wl-copy", &[] as &[&str]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["--clipboard", "--input"])
            ]
        );
    }

    #[test]
    fn clipboard_worker_does_not_block_and_limits_concurrent_jobs() {
        let mut clipboard = Clipboard::default();
        let started = Instant::now();
        assert!(
            clipboard
                .start_with("Captured title".into(), |text, cancel| {
                    assert_eq!(text, "Captured title");
                    while !cancel.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Ok(())
                })
                .unwrap()
        );
        assert!(
            !clipboard
                .start_with("Another title".into(), |_, _| panic!("duplicate worker"))
                .unwrap()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(clipboard.poll().is_none());
        drop(clipboard);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn helpers_that_never_read_input_or_exit_are_killed_within_the_deadline() {
        let cancel = AtomicBool::new(false);
        for text in ["title".to_owned(), "x".repeat(1024 * 1024)] {
            let started = Instant::now();
            let error = copy_with_command(
                "sh",
                &["-c", "sleep 60 & wait"],
                &text,
                Duration::from_millis(80),
                &cancel,
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_and_failed_helpers_fall_back_to_a_working_input_consumer() {
        let cancel = AtomicBool::new(false);
        let commands: &[(&str, &[&str])] = &[
            ("todoist-tui-nonexistent-clipboard-helper", &[]),
            ("sh", &["-c", "exit 9"]),
            (
                "sh",
                &["-c", "IFS= read -r line; [ \"$line\" = \"copied title\" ]"],
            ),
        ];
        copy_with_commands(commands, "copied title", Duration::from_secs(1), &cancel).unwrap();
        let timed_out_then_working: &[(&str, &[&str])] =
            &[("sh", &["-c", "sleep 60 & wait"]), commands[2]];
        copy_with_commands(
            timed_out_then_working,
            "copied title",
            Duration::from_millis(200),
            &cancel,
        )
        .unwrap();
        assert!(
            copy_with_commands(&commands[..1], "title", Duration::from_secs(1), &cancel)
                .unwrap_err()
                .to_string()
                .contains("Clipboard unavailable")
        );
    }
}

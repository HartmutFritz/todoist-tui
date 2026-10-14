use std::{
    io::{ErrorKind, Write},
    process::{Command, Stdio},
};

use anyhow::{Result, bail};

/// Copy text using the standard clipboard helper for the active Linux session.
///
/// Keeping this behind small external helpers avoids linking the application to
/// either a Wayland or X11 client library. It also lets the same binary work in
/// both display-server environments.
pub fn copy(text: &str) -> Result<()> {
    for (program, arguments) in clipboard_commands() {
        match copy_with_command(program, arguments, text) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }

    bail!("Clipboard unavailable: install wl-clipboard (Wayland), or xclip/xsel (X11)")
}

fn clipboard_commands() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
}

fn copy_with_command(program: &str, arguments: &[&str], text: &str) -> std::io::Result<bool> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let write_result = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()))
        .transpose();
    let status = child.wait()?;
    write_result?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_wayland_and_both_common_x11_helpers() {
        assert_eq!(
            clipboard_commands(),
            &[
                ("wl-copy", &[] as &[&str]),
                ("xclip", &["-selection", "clipboard"] as &[&str]),
                ("xsel", &["--clipboard", "--input"] as &[&str]),
            ]
        );
    }
}

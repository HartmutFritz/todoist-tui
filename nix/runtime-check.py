"""Exercise the installed wrapper, editor, and durable offline use in a real PTY."""

import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import time


binary, neovim, shell = sys.argv[1:]


class Terminal:
    def __init__(self, cwd, env):
        self.output = bytearray()
        self.cursor_queries = 0
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
        self.process = subprocess.Popen(
            [binary], stdin=slave, stdout=slave, stderr=slave, cwd=cwd, env=env,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
        )
        os.close(slave)

    def wait(self, predicate, description):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if predicate():
                return
            if select.select([self.master], [], [], 0.05)[0]:
                try:
                    self.output.extend(os.read(self.master, 65536))
                    queries = self.output.count(b"\x1b[6n")
                    while self.cursor_queries < queries:
                        self.send(b"\x1b[1;1R")
                        self.cursor_queries += 1
                except OSError:
                    pass
            if self.process.poll() is not None:
                if predicate():
                    return
                break
        text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", self.output)
        raise AssertionError(f"Timed out waiting for {description}: {text[-2000:]!r}")

    def send(self, data):
        os.write(self.master, data)

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait(timeout=5)
        os.close(self.master)


def exercise(root, custom_editor, certificates, save_task=False):
    root.mkdir()
    home = root / "home"
    config = home / ".config"
    nvim_config = config / "nvim"
    nvim_config.mkdir(parents=True)
    marker = root / "editor-loaded"
    (nvim_config / "init.lua").write_text(
        'vim.fn.writefile({"loaded"}, vim.env.EDITOR_MARKER)\n'
    )
    probe = root / "probe.json"
    bindir = root / "bin"
    bindir.mkdir()
    if custom_editor:
        shim = bindir / "nvim"
        shim.write_text(
            f"#!{sys.executable}\n"
            "import json, os, shutil, sys\n"
            "with open(os.environ['EDITOR_PROBE'], 'w') as f:\n"
            "    json.dump({'helpers': {name: shutil.which(name) for name in "
            "['wl-copy', 'xclip', 'xsel']}, 'file': os.environ.get('SSL_CERT_FILE'), "
            "'dir': os.environ.get('SSL_CERT_DIR')}, f)\n"
            f"os.execv({neovim!r}, [{neovim!r}] + sys.argv[1:])\n"
        )
        shim.chmod(0o755)
    env = {
        "HOME": str(home), "XDG_CONFIG_HOME": str(config),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_STATE_HOME": str(home / ".local/state"),
        "TERM": "xterm-256color", "LANG": "C.UTF-8",
        "PATH": str(bindir), "SHELL": shell,
        "EDITOR_MARKER": str(marker), "EDITOR_PROBE": str(probe),
        **certificates,
    }
    terminal = Terminal(root, env)
    try:
        terminal.wait(lambda: b"API token missing" in terminal.output, "local-only startup")
        terminal.send(b"a")
        terminal.wait(marker.exists, "user Neovim configuration")
        if custom_editor:
            result = json.loads(probe.read_text())
            assert all(result["helpers"].values()), result
            if "SSL_CERT_DIR" in certificates:
                assert result["dir"] == certificates["SSL_CERT_DIR"], result
                assert result["file"] is None, result
            else:
                expected = certificates.get("SSL_CERT_FILE", certificates.get("NIX_SSL_CERT_FILE"))
                if expected is not None:
                    assert result["file"] == expected, result
                else:
                    assert Path(result["file"]).is_file(), result
        # Let Neovim finish starting Insert mode after its init.lua executes.
        time.sleep(0.3)
        if save_task:
            terminal.send(b"Nix packaging smoke task")
            terminal.send(b"\x1b")
            time.sleep(0.15)
            terminal.send(b"\r")
            database = home / ".local/share/todoist-tui/todoist-tui.db"

            def saved():
                if not database.exists():
                    return False
                with sqlite3.connect(database) as connection:
                    return connection.execute(
                        "SELECT count(*) FROM tasks WHERE content = ?",
                        ("Nix packaging smoke task",),
                    ).fetchone()[0] == 1

            terminal.wait(saved, "durable offline task")
        else:
            terminal.send(b"\x1b")
            time.sleep(0.15)
            terminal.send(b":cq\r")
            terminal.wait(lambda: b"Edit cancelled" in terminal.output, "editor cancellation")
        terminal.send(b"Q")
        terminal.wait(lambda: terminal.process.poll() is not None, "normal quit")
        assert terminal.process.returncode == 0
    finally:
        terminal.close()
    if save_task:
        terminal = Terminal(root, env)
        try:
            terminal.wait(lambda: b"Nix packaging smoke task" in terminal.output, "task after restart")
            terminal.send(b"Q")
            terminal.wait(lambda: terminal.process.poll() is not None, "restart quit")
            assert terminal.process.returncode == 0
        finally:
            terminal.close()


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    exercise(root / "fallback", False, {}, save_task=True)
    exercise(root / "user-editor", True, {})
    exercise(root / "custom-ca", True, {
        "SSL_CERT_FILE": "/custom/ca.pem", "NIX_SSL_CERT_FILE": "/must-not-win.pem",
    })
    exercise(root / "empty-ca", True, {"SSL_CERT_FILE": ""})
    exercise(root / "nix-ca", True, {"NIX_SSL_CERT_FILE": "/custom/nix-ca.pem"})
    exercise(root / "ca-dir", True, {"SSL_CERT_DIR": "/custom/certs"})
print("Installed runtime checks passed")

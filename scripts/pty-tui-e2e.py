#!/usr/bin/env python3
"""Drive the real ovvc-tui through a PTY.

Unlike the ratatui `TestBackend` unit tests, this runs the process end to end:
crossterm raw mode, the alternate screen, the event loop, the daemon it spawns,
onboarding, a real message and `/quit`. The terminal is a 120x40 pseudo
terminal owned by this script, so no desktop or compositor is involved.

Usage: pty-tui-e2e.py <path-to-ovvc-tui>
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

ANSI = re.compile(rb"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07]*\x07|[@-_])")


def clean(data):
    return ANSI.sub(b"", data).decode("utf-8", "replace").replace("\r", "\n")


class PtyTui:
    def __init__(self, binary, argv_extra=(), cols=120, rows=40):
        self.binary = binary
        self.data_dir = tempfile.mkdtemp(prefix="ovvc-tui-pty-")
        self.cols = cols
        self.rows = rows
        self.buffer = b""
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.master = master
        self.proc = subprocess.Popen(
            [binary, "--data", self.data_dir, *argv_extra],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            close_fds=True,
            start_new_session=True,
        )
        os.close(slave)

    def _read(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.master], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.buffer += chunk

    def screen(self):
        return clean(self.buffer)

    def dump(self, label):
        text = self.screen()
        print(f"\n--- {label} (last 5000 chars) ---\n{text[-5000:]}", file=sys.stderr)

    def wait_for(self, needle, timeout=25.0, label=None):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self._read(0.1)
            if needle in self.screen():
                return
            if self.proc.poll() is not None:
                self.dump(f"process exited while waiting for {needle!r}")
                raise SystemExit(
                    f"ovvc-tui exited with {self.proc.returncode} before {needle!r} appeared"
                )
        self.dump(f"timeout waiting for {needle!r}")
        raise SystemExit(label or f"timed out waiting for {needle!r}")

    def send(self, text, delay=0.35):
        os.write(self.master, text.encode())
        self._read(delay)

    def send_key(self, key, delay=0.35):
        os.write(self.master, key)
        self._read(delay)

    def finish(self, timeout=15.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self._read(0.1)
            code = self.proc.poll()
            if code is not None:
                return code
        self.send_key(b"\x03", 0.2)
        try:
            return self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return self.proc.wait()

    def cleanup(self):
        try:
            if self.proc.poll() is None:
                self.proc.kill()
                self.proc.wait()
        finally:
            try:
                os.close(self.master)
            except OSError:
                pass
            # The TUI normally shuts down a daemon it spawned. If a failure
            # left one behind, stop only the daemon rooted at our temp dir.
            info = os.path.join(self.data_dir, "daemon.json")
            try:
                import json

                with open(info, encoding="utf-8") as handle:
                    pid = json.load(handle).get("pid")
                if pid:
                    os.kill(int(pid), signal.SIGTERM)
            except Exception:
                pass


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: pty-tui-e2e.py <path-to-ovvc-tui>")
    tui = PtyTui(sys.argv[1])
    try:
        tui.wait_for("What should people call you?", label="onboarding name prompt")
        tui.send("pty-tester\r")
        tui.wait_for("Privacy and security", label="privacy step")
        tui.send_key(b"\r")
        tui.wait_for("How do you want to start?", label="onboarding choice")
        tui.send_key(b"1")
        tui.wait_for("Name your commune", label="create-commune prompt")
        tui.send("pty-commune\r")
        tui.wait_for("pty-commune", label="chat after creating a commune")
        tui.wait_for("general", label="default #general channel")
        tui.wait_for("created", label="creation status")

        tui.send("/help\r")
        tui.wait_for("commands", label="help overlay")
        tui.wait_for("/history", label="help lists commands")
        tui.send_key(b"\x1b")

        tui.send("hello from the pty\r")
        tui.wait_for("hello from the pty", label="the message in the transcript")

        tui.send("/whoami\r")
        tui.wait_for("identity", label="identity overlay")
        tui.wait_for("pty-tester", label="display name in identity overlay")
        tui.send_key(b"\x1b")

        tui.send_key(b"\x03")
        code = tui.finish()
        if code != 0:
            tui.dump("after quit")
            raise SystemExit(f"ovvc-tui exited with {code}")
        print("pty TUI e2e: onboarding, chat, help, send, whoami and quit passed")
    finally:
        tui.cleanup()


if __name__ == "__main__":
    main()

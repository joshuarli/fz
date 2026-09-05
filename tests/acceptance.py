#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
#
# Behavioral cases are ported from fzy's MIT-licensed
# test/acceptance/acceptance_test.rb (Copyright 2014-2025 John Hawthorn).
"""End-to-end compatibility tests for fz.

The test is intentionally stdlib-only: tests/acceptance.rs supplies the built
binary and this file drives its real TTY through a private PTY.  The tiny ANSI
reader below understands only the sequences fzy emits; it is an assertion aid,
not a terminal emulator.

Upstream acceptance mapping:
  empty_list/one_item/two_items       -> test_empty_one_and_two_items
  editing/moving_text_cursor           -> test_editing_and_cursor_movement
  ctrl_d/ctrl_c                        -> test_cancel_and_no_match_enter_have_distinct_contracts
  down_arrow/up_arrow/bracketed_paste  -> test_navigation_and_bracketed_paste
  lines/prompt/show_scores/show_info   -> test_lines_prompt_info_scores_and_clipping
  worker_count/large_input             -> test_slow_stdin_and_large_input
  initial_query/non_interactive        -> test_initial_query_and_noninteractive_options
  unicode*                             -> test_unicode_editing_and_cursor_columns
  slow_stdin                           -> test_slow_stdin_and_large_input
"""

import codecs
import os
import pty
import selectors
import signal
import subprocess
import sys
import termios
import time
import unittest
import unicodedata
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else None
ESC = b"\x1b"
LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"


def width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in "WF" else 1


class Screen:
    """The subset of ANSI CSI fzy uses to redraw its prompt and result rows."""

    def __init__(self, rows=24, cols=80):
        self.rows, self.cols = rows, cols
        self.cells = [[" "] * cols for _ in range(rows)]
        self.y = self.x = 0
        self.raw = bytearray()
        self._stream = bytearray()
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.raw.extend(data)
        self._stream.extend(data)
        while self._stream:
            esc = self._stream.find(ESC)
            if esc < 0:
                self._text(bytes(self._stream))
                self._stream.clear()
                return
            if esc:
                self._text(bytes(self._stream[:esc]))
                del self._stream[:esc]
            if len(self._stream) < 2:
                return
            if self._stream[1] != ord("["):
                del self._stream[:2]
                continue
            final = next(
                (i for i in range(2, len(self._stream))
                 if 0x40 <= self._stream[i] <= 0x7e),
                None,
            )
            if final is None:
                return
            params = bytes(self._stream[2:final]).decode("ascii", "ignore")
            code = chr(self._stream[final])
            del self._stream[:final + 1]
            # Keep an incomplete UTF-8 sequence through SGR highlighting: a
            # terminal's text decoder does too.  A cursor/erase operation is
            # a redraw boundary, though; retaining a partial byte across it
            # would join bytes drawn at different screen positions (for
            # example rendering `> çç` after editing one `ç`).
            if code in "ABGK":
                self._decoder.reset()
            self._csi(params, code)

    def _text(self, data):
        for ch in self._decoder.decode(data):
            if ch == "\r":
                self.x = 0
            elif ch == "\n":
                self.y = min(self.rows - 1, self.y + 1)
            elif ord(ch) >= 0x20:
                w = width(ch)
                if self.x < self.cols:
                    self.cells[self.y][self.x] = ch
                    if w == 2 and self.x + 1 < self.cols:
                        self.cells[self.y][self.x + 1] = ""
                self.x += w

    def _csi(self, params, code):
        value = int(params) if params.isdigit() else 1
        if code == "A":
            self.y = max(0, self.y - value)
        elif code == "B":
            self.y = min(self.rows - 1, self.y + value)
        elif code == "G":
            self.x = max(0, value - 1)
        elif code == "K":
            for col in range(self.x, self.cols):
                self.cells[self.y][col] = " "
        # m and private wrap mode (?7h/?7l) only affect presentation.

    def lines(self):
        last = 0
        values = []
        for y, row in enumerate(self.cells):
            line = "".join(row).rstrip()
            values.append(line)
            if line:
                last = y + 1
        return values[:last]


def ignore_sighup():
    signal.signal(signal.SIGHUP, signal.SIG_IGN)


class PtyFz:
    def __init__(self, candidates=b"", args=(), rows=24, cols=80, close_stdin=True,
                 binary=None, ignore_hangup=False):
        self.master, self.slave = pty.openpty()
        import fcntl
        import struct
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        # macOS invalidates an idle slave FD when the child closes its last
        # handle.  The paired master reports the same shared terminal state
        # and remains queryable, so use it for the restoration assertion.
        self.original_termios = termios.tcgetattr(self.master)
        self.screen = Screen(rows, cols)
        self.proc = subprocess.Popen(
            [str(binary or BIN), *args, "--tty", os.ttyname(self.slave)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True, preexec_fn=ignore_sighup if ignore_hangup else None,
        )
        if candidates is not None:
            self.write_stdin(candidates)
            if close_stdin:
                self.close_stdin()

    def write_stdin(self, data):
        try:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()
            return True
        except BrokenPipeError:
            # A failing candidate binary may have exited before this test sends
            # its deliberately slow input.  The subsequent assertion reports
            # that behavioral failure without turning the harness into an error.
            return False

    def close_stdin(self):
        if self.proc.stdin and not self.proc.stdin.closed:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass

    def send(self, data):
        if self.master is None:
            raise AssertionError("cannot send input after PTY hangup")
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def drain(self, timeout=0.03):
        if self.master is None:
            return
        selector = selectors.DefaultSelector()
        try:
            selector.register(self.master, selectors.EVENT_READ)
            while selector.select(timeout):
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.screen.feed(data)
                timeout = 0
        finally:
            selector.close()

    def hangup(self):
        """Give the child a terminal EOF without closing its stdio pipes."""
        if self.master is not None:
            os.close(self.master)
            self.master = None

    def wait_for_lines(self, expected, timeout=2.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if self.screen.lines() == expected:
                return
            time.sleep(0.01)
        raise AssertionError("screen never matched\nexpected: %r\nactual:   %r\nraw: %r" %
                             (expected, self.screen.lines(), bytes(self.screen.raw[-400:])))

    def wait_for_prefix(self, expected, timeout=2.0):
        """Use only when a test intentionally leaves lower rows unspecified."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if self.screen.lines()[:len(expected)] == expected:
                return
            time.sleep(0.01)
        raise AssertionError("screen prefix never matched\nexpected: %r\nactual:   %r" %
                             (expected, self.screen.lines()))

    def wait_for_output(self, timeout=2.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if self.screen.raw:
                return
            time.sleep(0.01)
        raise AssertionError("fz did not render its initial interactive screen")

    def wait_for_redraw_after(self, raw_size, timeout=2.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if len(self.screen.raw) > raw_size:
                return
            time.sleep(0.01)
        raise AssertionError("fz did not redraw after SIGWINCH")

    def finish(self, keys=b"", timeout=3.0):
        if keys:
            self.send(keys)
        deadline = time.monotonic() + timeout
        while self.proc.poll() is None and time.monotonic() < deadline:
            self.drain()
            time.sleep(0.01)
        self.drain(0.1)
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
            raise AssertionError("fz did not exit after %r" % (keys,))
        stdout = self.proc.stdout.read()
        stderr = self.proc.stderr.read()
        return self.proc.returncode, stdout, stderr

    def restored(self):
        return termios.tcgetattr(self.master) == self.original_termios

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        for stream in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
            if stream and not stream.closed:
                stream.close()
        if self.master is not None:
            os.close(self.master)
            self.master = None
        os.close(self.slave)


def run_fz(args=(), data=b""):
    return subprocess.run([str(BIN), *args], input=data, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, check=False, timeout=5)


def pty_outcome(binary, candidates, args, keys):
    session = PtyFz(candidates, args, binary=binary)
    try:
        session.wait_for_output()
        code, stdout, stderr = session.finish(keys)
        return code, stdout, stderr, session.restored()
    finally:
        session.close()


def oracle_path():
    configured = os.environ.get("FZY_ORACLE")
    if configured:
        path = Path(configured).expanduser()
        if not path.is_file() or not os.access(path, os.X_OK):
            raise AssertionError("FZY_ORACLE is set but is not an executable: %s" % path)
        return path
    for path in (ROOT / "target/upstream/fzy", Path.home() / "d/fzy/build/fzy"):
        if path.is_file() and os.access(path, os.X_OK):
            return path
    return None


def deterministic_fixtures():
    # A fixed tiny LCG keeps this reproducible without a random dependency/API.
    state = 0xF25EED
    alphabet = "abcde/_.-XYZ0123"
    values = [
        "app/models/order", "app/models/zrder", "App/Models/Order",
        "src/fuzzy_match.rs", "src/fz/main.rs", "README.md", "foo", "foo",
        "fOo", "Français", "日本語", "a" * 1100, "a" * 1025,
    ]
    generated = []
    for _ in range(128):
        state = (1103515245 * state + 12345) & 0x7fffffff
        size = 4 + state % 19
        text = []
        for _ in range(size):
            state = (1103515245 * state + 12345) & 0x7fffffff
            text.append(alphabet[state % len(alphabet)])
        generated.append("".join(text))
    values.extend(generated)

    queries = ["", "does-not-exist", "FOO", "ç", "日本", "foo", "a" * 1025]
    for _ in range(100):
        state = (1103515245 * state + 12345) & 0x7fffffff
        candidate = generated[state % len(generated)]
        # Pick a nonempty ordered subset, making every generated query a real
        # subsequence while still varying gaps, punctuation, and case.
        indices = []
        for index in range(len(candidate)):
            state = (1103515245 * state + 12345) & 0x7fffffff
            if state & 1:
                indices.append(index)
        queries.append("".join(candidate[index] for index in indices) or candidate[0])
    return ("\n".join(values) + "\n").encode(), tuple(queries)


class Acceptance(unittest.TestCase):
    def test_batched_query_navigation_and_completion_use_fresh_results(self):
        session = self.session(b"foo\nbar\nbaz\n")
        session.wait_for_lines([">", "foo", "bar", "baz"])
        self.assert_exit(session, b"ba\x1b[B\r", 0, b"baz\n")
        session = self.session(b"foo\nbar\nbaz\n")
        session.wait_for_lines([">", "foo", "bar", "baz"])
        self.assert_exit(session, b"br\t\r", 0, b"bar\n")

    @classmethod
    def setUpClass(cls):
        if BIN is None:
            raise AssertionError("usage: acceptance.py /path/to/fz")
        if not BIN.is_file():
            raise AssertionError("fz binary does not exist: %s" % BIN)

    def session(self, candidates=b"", args=(), **kwargs):
        session = PtyFz(candidates, args, **kwargs)
        self.addCleanup(session.close)
        return session

    def assert_exit(self, session, keys, code, stdout):
        actual_code, actual_stdout, stderr = session.finish(keys)
        self.assertEqual(actual_code, code, stderr.decode("utf-8", "replace"))
        self.assertEqual(actual_stdout, stdout)
        self.assertTrue(session.restored(), "fz did not restore the TTY termios state")

    def test_empty_one_and_two_items(self):
        empty = self.session()
        empty.wait_for_lines([">"])
        empty.send("tz")
        empty.wait_for_lines(["> tz"])
        self.assert_exit(empty, b"\r", 0, b"tz\n")

        one = self.session(b"test\n")
        one.wait_for_lines([">", "test"])
        one.send("tz")
        one.wait_for_lines(["> tz"])
        self.assert_exit(one, b"\r", 0, b"tz\n")

        two = self.session(b"test\nfoo\n")
        two.wait_for_lines([">", "test", "foo"])
        two.send("t")
        two.wait_for_lines(["> t", "test"])
        self.assert_exit(two, b"z\r", 0, b"tz\n")

    def test_editing_and_cursor_movement(self):
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        session.send("foo bar baz")
        session.wait_for_lines(["> foo bar baz"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 13))
        session.send(b"\x08")
        session.wait_for_lines(["> foo bar ba"])
        session.send(b"\x17")
        session.wait_for_lines(["> foo bar"])
        session.send(b"\x15")
        session.wait_for_lines([">", "foo", "bar"])

        session.send("br")
        session.wait_for_lines(["> br", "bar"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 4))
        session.send(LEFT)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 3))
        session.send("a")
        session.wait_for_lines(["> bar", "bar"])
        session.send(b"\x01foo")
        session.wait_for_lines(["> foobar"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 5))
        session.send(b"\x05baz")
        session.wait_for_lines(["> foobarbaz"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 11))
        self.assert_exit(session, b"\x03", 1, b"")

    def test_cancel_and_no_match_enter_have_distinct_contracts(self):
        for key in (b"\x03", b"\x04"):
            session = self.session(b"foo\nbar\n")
            session.wait_for_lines([">", "foo", "bar"])
            session.send("foo")
            session.wait_for_lines(["> foo", "foo"])
            self.assert_exit(session, key, 1, b"")

        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        self.assert_exit(session, b"z\r", 0, b"z\n")

    def test_navigation_and_bracketed_paste(self):
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        self.assert_exit(session, b"\x1b[A\r", 0, b"bar\n")
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        self.assert_exit(session, b"\x1bOA\x1b[B\r", 0, b"foo\n")
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        session.send(b"\x1b[200~foo\x1b[201~")
        session.wait_for_lines(["> foo", "foo"])
        self.assertIn(b"\x1b[33m", session.screen.raw, "matched characters need a highlight SGR")
        self.assert_exit(session, b"\r", 0, b"foo\n")

    def test_escape_timeout_and_fragmented_arrow_sequence(self):
        # ESC remains ambiguous for KEYTIMEOUT; by itself it is cancel, while
        # bytes completing an arrow sequence before that timeout are navigation.
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        session.send(ESC)
        self.assert_exit(session, b"", 1, b"")

        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        session.send(ESC)
        time.sleep(0.005)  # Strictly below fzy's 25 ms ambiguous-key timeout.
        session.send(b"[A")
        self.assert_exit(session, b"\r", 0, b"bar\n")

    def test_sigwinch_redraw_preserves_editor_and_tty_contract(self):
        session = self.session(b"foo\nbar\n")
        session.wait_for_lines([">", "foo", "bar"])
        session.send(b"\x1b[A")
        session.drain(0.1)
        before_lines = session.screen.lines()
        before_cursor = (session.screen.y, session.screen.x)
        before_raw_size = len(session.screen.raw)
        os.kill(session.proc.pid, signal.SIGWINCH)
        session.wait_for_redraw_after(before_raw_size)
        self.assertEqual(session.screen.lines(), before_lines)
        self.assertEqual((session.screen.y, session.screen.x), before_cursor)
        self.assert_exit(session, b"\r", 0, b"bar\n")

    def test_tty_eof_exits_failure_without_stdout(self):
        # This deliberately destroys the PTY master, so its device state is no
        # longer meaningful and this test intentionally does not call restored.
        session = self.session(b"foo\n", ignore_hangup=True)
        session.wait_for_lines([">", "foo"])
        session.hangup()
        code, stdout, _stderr = session.finish()
        self.assertEqual(code, 1)
        self.assertEqual(stdout, b"")

    def test_navigation_boundaries_scrolling_and_info_cleanup(self):
        data = b"1\n2\n3\n4\n"
        session = self.session(data, ("-l3",))
        session.wait_for_lines([">", "1", "2", "3"])
        self.assert_exit(session, b"\x1b[5~\r", 0, b"1\n")

        session = self.session(data, ("-l3",))
        session.wait_for_lines([">", "1", "2", "3"])
        self.assert_exit(session, b"\x1b[6~\r", 0, b"4\n")

        session = self.session(data, ("-l3",))
        session.wait_for_lines([">", "1", "2", "3"])
        session.send(b"\x0e\x0e")
        session.wait_for_lines([">", "2", "3", "4"])
        self.assert_exit(session, b"\x03", 1, b"")

        for key, code, output in ((b"\r", 0, b"foo\n"), (b"\x03", 1, b"")):
            session = self.session(b"foo\nbar\n", ("-i",))
            session.wait_for_lines([">", "[2/2]", "foo", "bar"])
            self.assert_exit(session, key, code, output)
            self.assertEqual(session.screen.lines(), [])
            self.assertEqual((session.screen.y, session.screen.x), (0, 0))

    def test_lines_prompt_info_scores_and_clipping(self):
        ten = b"".join((str(i) + "\n").encode() for i in range(1, 11))
        twenty = b"".join((str(i) + "\n").encode() for i in range(1, 21))
        session = self.session(twenty)
        session.wait_for_lines([">", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(ten, ("-l", "5"))
        session.wait_for_lines([">", "1", "2", "3", "4", "5"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(ten, ("--lines=5",))
        session.wait_for_lines([">", "1", "2", "3", "4", "5"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(args=("-p", "C:\\"))
        session.wait_for_lines(["C:\\"])
        session.send("foo")
        session.wait_for_lines(["C:\\foo"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(args=("--prompt=foo bar ",))
        session.wait_for_lines(["foo bar"])
        session.send("baz")
        session.wait_for_lines(["foo bar baz"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(b"foo\nbar\nbaz\n", ("-i",))
        session.wait_for_lines([">", "[3/3]", "foo", "bar", "baz"])
        session.send("ba")
        session.wait_for_lines(["> ba", "[2/3]", "bar", "baz"])
        session.send("q")
        session.wait_for_lines(["> baq", "[0/3]"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(b"foo\nbar\n", ("-s",))
        session.send("foo")
        session.wait_for_lines(["> foo", "(  inf) foo"])
        self.assertIn(b"\x1b[33m", session.screen.raw)
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(b"foo\nbar\n", ("--show-scores",))
        session.send("f")
        session.wait_for_lines(["> f", "( 0.89) foo"])
        self.assert_exit(session, b"\x03", 1, b"")

        ascii_value = "LongStringOfText" * 6
        unicode_value = "ＬｏｎｇＳｔｒｉｎｇＯｆＴｅｘｔ" * 3
        session = self.session((ascii_value + "\n" + unicode_value + "\n").encode())
        session.wait_for_lines([">", ascii_value[:80], unicode_value[:40]])
        self.assert_exit(session, b"\x03", 1, b"")

    def test_unicode_editing_and_cursor_columns(self):
        session = self.session("English\nFrançais\n日本語\n".encode())
        session.wait_for_lines([">", "English", "Français", "日本語"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 2))
        session.send("ç")
        session.wait_for_lines(["> ç", "Français"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 3))
        self.assert_exit(session, b"\r", 0, "Français\n".encode())

        session = self.session()
        session.send("Français")
        session.wait_for_lines(["> Français"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 10))
        session.send(b"\x08" * 3)
        session.wait_for_lines(["> Franç"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 7))
        session.send(b"\x08ce")
        session.wait_for_lines(["> France"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session()
        session.send("Je parle Français")
        session.wait_for_lines(["> Je parle Français"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 19))
        session.send(b"\x17")
        session.wait_for_lines(["> Je parle"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 11))
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session()
        session.send("Français")
        session.wait_for_lines(["> Français"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 10))
        session.send(LEFT * 5)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 5))
        session.send(RIGHT * 3)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 8))
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session()
        session.send("日本語")
        session.wait_for_lines(["> 日本語"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 8))
        session.send(LEFT)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 6))
        session.send(LEFT)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 4))
        session.send(LEFT * 2)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 2))
        session.send(RIGHT * 4)
        session.drain(0.1)
        self.assertEqual((session.screen.y, session.screen.x), (0, 8))
        session.send(b"\x17")
        session.wait_for_lines([">"])
        self.assertEqual((session.screen.y, session.screen.x), (0, 2))
        session.send("日本語")
        session.wait_for_lines(["> 日本語"])
        session.send(b"\x08")
        session.wait_for_lines(["> 日本"])
        session.send(b"\x08")
        session.wait_for_lines(["> 日"])
        session.send(b"\x08")
        session.wait_for_lines([">"])
        self.assert_exit(session, b"\x03", 1, b"")

    def test_slow_stdin_and_large_input(self):
        session = self.session(None)
        time.sleep(0.08)
        session.send("b\r")
        session.write_stdin(b"aa\nbc\nbd\n")
        session.close_stdin()
        self.assert_exit(session, b"", 0, b"bc\n")

        values = b"".join((str(i) + "\n").encode() for i in range(100000))
        session = self.session(values, ("-j200", "-l3"))
        session.send("34")
        session.wait_for_lines(["> 34", "34", "340", "341"], timeout=5)
        session.send("5")
        session.wait_for_lines(["> 345", "345", "3450", "3451"], timeout=5)
        session.send("z")
        session.wait_for_lines(["> 345z"], timeout=5)
        self.assert_exit(session, b"\x03", 1, b"")

    def test_initial_query_and_noninteractive_options(self):
        session = self.session(b"foo\nbar\n", ("-q", "fo"))
        session.wait_for_lines(["> fo", "foo"])
        session.send("o")
        session.wait_for_lines(["> foo", "foo"])
        session.send("o")
        session.wait_for_lines(["> fooo"])
        self.assert_exit(session, b"\x03", 1, b"")
        session = self.session(b"foo\nbar\n", ("--query=asdf",))
        session.wait_for_lines(["> asdf"])
        self.assert_exit(session, b"\x03", 1, b"")

        data = b"foo\nbar\nbaz\n"
        result = run_fz(("-e", "ba"), data)
        self.assertEqual((result.returncode, result.stdout), (0, b"bar\nbaz\n"))
        result = run_fz(("-0", "-e", "ba"), b"foo\x00bar\x00baz\x00")
        self.assertEqual((result.returncode, result.stdout), (0, b"bar\nbaz\n"))
        one = run_fz(("-j1", "-e", "ba"), data)
        many = run_fz(("-j", "200", "-e", "ba"), data)
        self.assertEqual(one.stdout, many.stdout)
        self.assertNotEqual(run_fz(("-j", "nope", "-e", "ba"), data).returncode, 0)
        self.assertNotEqual(run_fz(("-l", "2", "-e", "ba"), data).returncode, 0)
        help_result = run_fz(("--help",))
        self.assertEqual(help_result.returncode, 0)
        help_text = help_result.stdout + help_result.stderr
        for option in (b"--lines", b"--prompt", b"--query", b"--show-matches",
                       b"--tty", b"--show-scores", b"--read-null", b"--show-info",
                       b"--help", b"--version"):
            self.assertIn(option, help_text)
        self.assertNotIn(b"benchmark", help_text.lower())
        # fz deliberately keeps -j as a compatibility parser input but makes
        # no claim to run a worker pool.  Upstream fzy documents its pool, so
        # do not apply this fz-specific help identity check to the oracle.
        if BIN.name != "fzy":
            self.assertIn(b"Usage: fz", help_text)
            self.assertIn(b"Maximum workers", help_text)
            self.assertNotIn(b"# of cpus", help_text.lower())
        version = run_fz(("--version",))
        self.assertEqual(version.returncode, 0)
        self.assertTrue(version.stdout)

    def test_differential_noninteractive_scores_and_ranks(self):
        oracle = oracle_path()
        if oracle is None:
            self.skipTest("differential oracle unavailable (set FZY_ORACLE or build target/upstream/fzy)")
        data, queries = deterministic_fixtures()
        for query in queries:
            ours = run_fz(("-e", query, "-s"), data)
            upstream = subprocess.run([str(oracle), "-e", query, "-s"], input=data,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
            self.assertEqual(ours.returncode, upstream.returncode, query)
            self.assertEqual(ours.stderr, upstream.stderr, query)
            self.assertEqual(ours.stdout, upstream.stdout,
                             "rank/score mismatch for deterministic query %r" % query)

    def test_differential_interactive_key_outcomes(self):
        oracle = oracle_path()
        if oracle is None:
            self.skipTest("differential oracle unavailable (set FZY_ORACLE or build target/upstream/fzy)")
        cases = (
            (b"foo\nbar\n", (), b"foo\r"),
            (b"foo\nbar\n", (), b"\x1b[A\r"),
            (b"foo\nbar\n", (), b"br\x1b[Da\r"),
            (b"foo\nbar\n", (), b"z\r"),
            (b"foo\nbar\n", (), b"\x03"),
            ("English\nFrançais\n日本語\n".encode(), (), "ç\r".encode()),
        )
        for candidates, args, keys in cases:
            ours = pty_outcome(BIN, candidates, args, keys)
            upstream = pty_outcome(oracle, candidates, args, keys)
            self.assertEqual(ours[:2], upstream[:2], (args, keys))
            self.assertTrue(ours[3], "fz did not restore termios after %r" % (keys,))


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]], verbosity=2)

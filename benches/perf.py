#!/usr/bin/env python3
"""Repeatable release-CLI and terminal benchmarks for fz against upstream fzy.

Rustybench in ``benches/bench.rs`` measures in-process matching, parsing, and
allocation behavior. This script deliberately keeps the work that must cross a
process boundary here: actual release binaries, upstream's worker settings,
child CPU and peak RSS, output compatibility, and PTY redraw latency.
"""

from __future__ import annotations

import argparse
import codecs
import dataclasses
import fcntl
import hashlib
import json
import os
import pty
import selectors
import signal
import statistics
import subprocess
import sys
import tempfile
import termios
import time
import struct
import unicodedata
from pathlib import Path
from typing import BinaryIO, Dict, Iterable, List, Optional, Sequence, Tuple


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FZ = ROOT / "target/release/fz"
DEFAULT_FZY = ROOT / "target/upstream/fzy"
# v2 leaves the original directory intact: earlier smoke invocations used its
# small.txt name for a deliberately smaller fixture, so treating it as a normal
# fixture would make a later normal run fail its content-integrity check.
DEFAULT_DATA_DIR = ROOT / "target/benchmarks/fzy-perf-v2"
LARGE_RECORDS = 100_000
SMALL_RECORDS = 128
NO_MATCH = "QZXQZXQZX"
PATH_EXACT = "src/fz/benchmark/perf_case.py"
PATH_LONGER = "src/fz/benchmark/performance_snapshot.py"


@dataclasses.dataclass(frozen=True)
class Query:
    """One useful filter outcome, checked before its timings are accepted."""

    name: str
    text: str
    expected: str  # ``some``, ``none``, or ``exact`` (the first result is text).


@dataclasses.dataclass(frozen=True)
class Dataset:
    name: str
    description: str
    contents: bytes
    queries: Tuple[Query, ...]


@dataclasses.dataclass(frozen=True)
class Sample:
    wall_seconds: float
    user_seconds: float
    system_seconds: float
    cpu_seconds: float
    peak_rss_bytes: int
    returncode: int


@dataclasses.dataclass(frozen=True)
class PreparedDataset:
    dataset: Dataset
    path: Path
    sha256: str


class BenchmarkError(RuntimeError):
    pass


@dataclasses.dataclass(frozen=True)
class TerminalExpectation:
    """The completed result region expected for one interactive query."""

    query: str
    count: int
    total: int
    top_rows: Tuple[str, ...]


def display_width(character: str) -> int:
    if unicodedata.combining(character):
        return 0
    return 2 if unicodedata.east_asian_width(character) in "WF" else 1


class Screen:
    """The ANSI subset fzy uses to redraw its prompt, info, and result rows."""

    def __init__(self, rows: int = 24, cols: int = 120) -> None:
        self.rows, self.cols = rows, cols
        self.cells = [[" "] * cols for _ in range(rows)]
        self.y = self.x = 0
        self._stream = bytearray()
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data: bytes) -> None:
        self._stream.extend(data)
        while self._stream:
            escape = self._stream.find(b"\x1b")
            if escape < 0:
                self._text(bytes(self._stream))
                self._stream.clear()
                return
            if escape:
                self._text(bytes(self._stream[:escape]))
                del self._stream[:escape]
            if len(self._stream) < 2:
                return
            if self._stream[1] != ord("["):
                del self._stream[:2]
                continue
            final = next(
                (index for index in range(2, len(self._stream)) if 0x40 <= self._stream[index] <= 0x7E),
                None,
            )
            if final is None:
                return
            params = bytes(self._stream[2:final]).decode("ascii", "ignore")
            code = chr(self._stream[final])
            del self._stream[:final + 1]
            # Cursor and erase commands delimit separate terminal redraws. Do
            # not join a partial UTF-8 sequence across that boundary.
            if code in "ABGK":
                self._decoder.reset()
            self._csi(params, code)

    def _text(self, data: bytes) -> None:
        for character in self._decoder.decode(data):
            if character == "\r":
                self.x = 0
            elif character == "\n":
                self.y = min(self.rows - 1, self.y + 1)
            elif ord(character) >= 0x20:
                width = display_width(character)
                if self.x < self.cols:
                    self.cells[self.y][self.x] = character
                    if width == 2 and self.x + 1 < self.cols:
                        self.cells[self.y][self.x + 1] = ""
                self.x += width

    def _csi(self, params: str, code: str) -> None:
        value = int(params) if params.isdigit() else 1
        if code == "A":
            self.y = max(0, self.y - value)
        elif code == "B":
            self.y = min(self.rows - 1, self.y + value)
        elif code == "G":
            self.x = max(0, value - 1)
        elif code == "K":
            for column in range(self.x, self.cols):
                self.cells[self.y][column] = " "

    def line(self, row: int) -> str:
        return "".join(self.cells[row]).rstrip()

    def matches(self, expected: TerminalExpectation) -> bool:
        """Require the query, count, top rows, and cleared trailing rows.

        fzy immediately draws each typed input byte with the old candidate
        results, then searches and redraws after its input batch. A new prompt
        alone therefore cannot finish a latency sample.
        """

        prompt = ">" if not expected.query else "> " + expected.query
        if self.line(0) != prompt:
            return False
        if self.line(1) != f"[{expected.count}/{expected.total}]":
            return False
        for index, row in enumerate(expected.top_rows):
            if self.line(index + 2) != row:
                return False
        for index in range(len(expected.top_rows), 3):
            if self.line(index + 2):
                return False
        return True


def mixed_word(index: int, lane: int) -> int:
    """Return deterministic, well-mixed counter output without an LCG."""

    value = (index + 0x9E3779B97F4A7C15 + lane * 0xBF58476D1CE4E5B9) & ((1 << 64) - 1)
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & ((1 << 64) - 1)
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & ((1 << 64) - 1)
    return value ^ (value >> 31)


def append_path(output: bytearray, index: int) -> None:
    """Append one varied but familiar source-tree path.

    Component selection uses separate SplitMix64 lanes, rather than the low
    bits of a linear congruential generator, so nearby records do not repeat a
    short component cycle.
    """

    roots = ("src", "packages", "services", "tools", "tests", "docs", "vendor")
    areas = ("auth", "catalog", "editor", "index", "network", "storage", "terminal")
    stems = ("command", "config", "handler", "model", "parser", "report", "snapshot")
    extensions = ("rs", "toml", "md", "json", "py")
    choose = lambda values, lane: values[mixed_word(index, lane) % len(values)]
    output.extend(choose(roots, 0).encode())
    output.extend(b"/")
    output.extend(choose(areas, 1).encode())
    output.extend(b"/")
    output.extend(choose(areas, 2).encode())
    output.extend(f"_{mixed_word(index, 3) & 0xFFFFFF:06x}/".encode())
    output.extend(choose(stems, 4).encode())
    output.extend(f"_{mixed_word(index, 5) & 0xFFFF:04x}.".encode())
    output.extend(choose(extensions, 6).encode())
    output.extend(b"\n")


def path_input(records: int) -> bytes:
    output = bytearray()
    if records:
        output.extend(PATH_EXACT.encode() + b"\n")
    if records > 1:
        output.extend(PATH_LONGER.encode() + b"\n")
    for index in range(2, records):
        append_path(output, index)
    return bytes(output)


def numeric_input(records: int) -> bytes:
    output = bytearray(records * 6)
    output.clear()
    for index in range(records):
        # 7,919 is coprime with 100,000, producing a deterministic permutation.
        output.extend(f"{(index * 7919 + 31415) % 100000:05d}\n".encode())
    return bytes(output)


def long_candidate(length: int, identifier: int) -> bytes:
    candidate = bytearray(f"long-candidate-{identifier:03d}/needle-{identifier:03d}/".encode())
    fill = b"abcdefghijklmnopqrstuvwxyz0123456789/_-."
    while len(candidate) < length:
        candidate.extend(fill[: min(len(fill), length - len(candidate))])
    return bytes(candidate)


def long_input(records: int) -> bytes:
    lengths = (1000, 1023, 1024, 1025, 1152, 1536)
    output = bytearray()
    for index in range(records):
        output.extend(long_candidate(lengths[index % len(lengths)], index))
        output.extend(b"\n")
    return bytes(output)


def make_datasets(records: int = LARGE_RECORDS, small_records: int = SMALL_RECORDS) -> Dict[str, Dataset]:
    small_contents = bytearray(path_input(small_records))
    small_contents.extend(b"README.md\napp/models/order.py\n")
    long_exact = long_candidate(1024, 44).decode("ascii")
    return {
        "small": Dataset(
            "small",
            "130 representative paths and filenames",
            bytes(small_contents),
            (
                Query("empty", "", "some"),
                Query("exact", "app/models/order.py", "exact"),
                Query("selective", "amor", "some"),
                Query("dense", "a", "some"),
                Query("no-match", NO_MATCH, "none"),
                Query("longer", PATH_LONGER, "exact"),
            ),
        ),
        "numeric_100k": Dataset(
            "numeric_100k",
            "100,000 distinct five-digit records",
            numeric_input(records),
            (
                Query("empty", "", "some"),
                Query("exact", "27182", "exact"),
                Query("selective", "987", "some"),
                Query("dense", "0", "some"),
                Query("no-match", "x", "none"),
                Query("longer", "31415", "exact"),
            ),
        ),
        "paths_100k": Dataset(
            "paths_100k",
            "100,000 deterministic, varied source-tree paths",
            path_input(records),
            (
                Query("empty", "", "some"),
                Query("exact", PATH_EXACT, "exact"),
                Query("selective", "src/fz/bench", "some"),
                Query("dense", "src", "some"),
                Query("no-match", NO_MATCH, "none"),
                Query("longer", PATH_LONGER, "exact"),
            ),
        ),
        "long_boundary": Dataset(
            "long_boundary",
            "128 candidates at 1000, 1023, 1024, 1025, 1152, and 1536 bytes",
            long_input(small_records),
            (
                Query("empty", "", "some"),
                Query("exact", long_exact, "exact"),
                Query("selective", "needle-044", "some"),
                Query("dense", "a", "some"),
                Query("no-match", NO_MATCH, "none"),
                Query("longer", "long-candidate-044/needle-044/abcdefghijklmnop", "some"),
            ),
        ),
    }


def sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def prepare_dataset(data_dir: Path, dataset: Dataset) -> PreparedDataset:
    """Materialize and verify input before every timed subprocess starts."""

    data_dir.mkdir(parents=True, exist_ok=True)
    path = data_dir / f"{dataset.name}.txt"
    digest = hashlib.sha256(dataset.contents).hexdigest()
    if path.exists():
        existing = sha256_path(path)
        if existing != digest:
            raise BenchmarkError(
                f"fixture {path} has digest {existing}, expected {digest}; use a new --data-dir"
            )
    else:
        with tempfile.NamedTemporaryFile("wb", dir=data_dir, prefix=f".{dataset.name}.", delete=False) as temp:
            temp.write(dataset.contents)
            temporary_path = Path(temp.name)
        os.replace(temporary_path, path)
    return PreparedDataset(dataset, path, digest)


def validate_binary(path: Path, label: str) -> Path:
    path = path.expanduser().resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise BenchmarkError(f"{label} is not an executable: {path}")
    return path


def sample_from_wait(
    process: subprocess.Popen[bytes], status: int, usage: object, started: float
) -> Sample:
    ended = time.perf_counter()
    returncode = os.waitstatus_to_exitcode(status)
    process.returncode = returncode
    # ru_maxrss is bytes on Darwin and KiB on Linux. Keep the report unit stable.
    rss = int(usage.ru_maxrss)
    if sys.platform != "darwin":
        rss *= 1024
    user = float(usage.ru_utime)
    system = float(usage.ru_stime)
    return Sample(ended - started, user, system, user + system, rss, returncode)


def wait_for_process(process: subprocess.Popen[bytes], started: float) -> Sample:
    while True:
        try:
            _pid, status, usage = os.wait4(process.pid, 0)
            return sample_from_wait(process, status, usage, started)
        except InterruptedError:
            continue


def run_child(
    command: Sequence[str], input_path: Path, stdout: BinaryIO, stderr: BinaryIO
) -> Sample:
    # Input has already been prepared; perf_counter begins immediately before spawn.
    with input_path.open("rb") as input_stream:
        started = time.perf_counter()
        process = subprocess.Popen(
            list(command), stdin=input_stream, stdout=stdout, stderr=stderr, close_fds=True
        )
        return wait_for_process(process, started)


def command(binary: Path, query: Query, mode: str) -> List[str]:
    arguments = [str(binary), "-e", query.text]
    if mode == "-j1":
        arguments.append("-j1")
    return arguments


def scored_command(binary: Path, query: Query, mode: str) -> List[str]:
    """Use fzy's non-interactive scores to prepare an interactive oracle."""

    arguments = [str(binary), "-e", query.text, "-s"]
    if mode == "-j1":
        arguments.append("-j1")
    return arguments


def terminal_expectation_from_output(path: Path, query: Query, total: int) -> TerminalExpectation:
    count = 0
    top_rows: List[str] = []
    with path.open("rb") as stream:
        for line in stream:
            score, separator, candidate = line.partition(b"\t")
            if not separator:
                raise BenchmarkError(f"oracle score output has no tab for {query.name}: {line[:120]!r}")
            # Keep score parsing structural: it proves the ranking came from
            # fzy's scored output, while the terminal itself displays choices.
            try:
                float(score)
            except ValueError as error:
                raise BenchmarkError(f"invalid oracle score for {query.name}: {score!r}") from error
            count += 1
            if len(top_rows) < 3:
                top_rows.append(candidate.rstrip(b"\n").decode("utf-8", "replace"))
    return TerminalExpectation(query.text, count, total, tuple(top_rows))


def prepare_terminal_expectations(
    prepared: PreparedDataset, fzy: Path
) -> Dict[Tuple[str, str], TerminalExpectation]:
    """Prepare expected counts and ranked terminal rows before timing a PTY."""

    total = prepared.dataset.contents.count(b"\n")
    expectations: Dict[Tuple[str, str], TerminalExpectation] = {}
    with tempfile.TemporaryDirectory(prefix="fz-perf-terminal-oracle-") as directory:
        root = Path(directory)
        for mode in ("default", "-j1"):
            for query in prepared.dataset.queries:
                output_path = root / f"{mode}-{query.name}.out"
                error_path = root / f"{mode}-{query.name}.err"
                with output_path.open("wb") as out, error_path.open("wb") as err:
                    sample = run_child(scored_command(fzy, query, mode), prepared.path, out, err)
                if sample.returncode != 0:
                    error = error_path.read_bytes()[:2000].decode("utf-8", "replace")
                    raise BenchmarkError(f"fzy oracle failed for interactive {query.name}/{mode}: {error}")
                expectation = terminal_expectation_from_output(output_path, query, total)
                if query.expected == "none" and expectation.count:
                    raise BenchmarkError(f"interactive oracle found {expectation.count} unexpected {query.name} matches")
                if query.expected != "none" and not expectation.count:
                    raise BenchmarkError(f"interactive oracle found no {query.name} match")
                expectations[(mode, query.name)] = expectation
    for mode in ("default", "-j1"):
        empty = expectations[(mode, "empty")]
        for query in prepared.dataset.queries:
            if query.name == "empty":
                continue
            expected = expectations[(mode, query.name)]
            if expected.count == empty.count and expected.top_rows == empty.top_rows:
                raise BenchmarkError(
                    f"interactive {query.name}/{mode} cannot be distinguished from the empty result state"
                )
    return expectations


def file_identity(path: Path) -> Tuple[int, str]:
    return path.stat().st_size, sha256_path(path)


def validate_output(path: Path, query: Query) -> None:
    size = path.stat().st_size
    if query.expected == "none":
        if size:
            raise BenchmarkError(f"{query.name} should have no matches, but {path} has {size} output bytes")
        return
    if not size:
        raise BenchmarkError(f"{query.name} should have a match, but {path} is empty")
    if query.expected == "exact":
        with path.open("rb") as stream:
            first = stream.readline()
        expected = query.text.encode() + b"\n"
        if first != expected:
            raise BenchmarkError(
                f"{query.name} should rank its exact candidate first: got {first[:120]!r}, expected {expected[:120]!r}"
            )


def verify_pair(prepared: PreparedDataset, query: Query, mode: str, fz: Path, fzy: Path) -> None:
    """Compare full output outside measured runs without ever using a pipe."""

    with tempfile.TemporaryDirectory(prefix="fz-perf-verify-") as directory:
        root = Path(directory)
        ours, upstream, ours_error, upstream_error = (
            root / "fz.out", root / "fzy.out", root / "fz.err", root / "fzy.err"
        )
        with ours.open("wb") as out, ours_error.open("wb") as err:
            ours_sample = run_child(command(fz, query, mode), prepared.path, out, err)
        with upstream.open("wb") as out, upstream_error.open("wb") as err:
            upstream_sample = run_child(command(fzy, query, mode), prepared.path, out, err)
        for label, sample, error_path in (
            ("fz", ours_sample, ours_error), ("fzy", upstream_sample, upstream_error),
        ):
            if sample.returncode != 0:
                error = error_path.read_bytes()[:2000].decode("utf-8", "replace")
                raise BenchmarkError(
                    f"{label} failed for {prepared.dataset.name}/{query.name}/{mode}: {error}"
                )
        validate_output(ours, query)
        validate_output(upstream, query)
        ours_id, upstream_id = file_identity(ours), file_identity(upstream)
        if ours_id != upstream_id:
            raise BenchmarkError(
                f"output mismatch for {prepared.dataset.name}/{query.name}/{mode}: "
                f"fz {ours_id[0]} bytes {ours_id[1]}, fzy {upstream_id[0]} bytes {upstream_id[1]}"
            )


def timed_sample(prepared: PreparedDataset, binary: Path, query: Query, mode: str) -> Sample:
    with open(os.devnull, "wb") as sink:
        sample = run_child(command(binary, query, mode), prepared.path, sink, sink)
    if sample.returncode != 0:
        raise BenchmarkError(f"{binary.name} failed for {prepared.dataset.name}/{query.name}/{mode}")
    return sample


def median_sample(samples: Sequence[Sample]) -> Dict[str, float]:
    return {
        "wall_seconds": statistics.median(sample.wall_seconds for sample in samples),
        "user_seconds": statistics.median(sample.user_seconds for sample in samples),
        "system_seconds": statistics.median(sample.system_seconds for sample in samples),
        "cpu_seconds": statistics.median(sample.cpu_seconds for sample in samples),
        "peak_rss_bytes": statistics.median(sample.peak_rss_bytes for sample in samples),
    }


def measure_noninteractive(
    datasets: Sequence[PreparedDataset], fz: Path, fzy: Path, warmups: int, repetitions: int
) -> Dict[Tuple[str, str, str, str], List[Sample]]:
    measurements: Dict[Tuple[str, str, str, str], List[Sample]] = {}
    for prepared in datasets:
        for query in prepared.dataset.queries:
            for mode in ("default", "-j1"):
                for warmup in range(warmups):
                    binaries = (("fz", fz), ("fzy", fzy))
                    if warmup % 2:
                        binaries = tuple(reversed(binaries))
                    for _label, binary in binaries:
                        timed_sample(prepared, binary, query, mode)
                for repetition in range(repetitions):
                    # Alternate immediate order to avoid systematically giving one
                    # binary the warmer filesystem and executable cache.
                    binaries = (("fz", fz), ("fzy", fzy))
                    if repetition % 2:
                        binaries = tuple(reversed(binaries))
                    for label, binary in binaries:
                        key = (prepared.dataset.name, query.name, mode, label)
                        measurements.setdefault(key, []).append(timed_sample(prepared, binary, query, mode))
    return measurements


def wait_for_render(master: int, screen: Screen, expected: TerminalExpectation, timeout: float) -> None:
    """Wait until the complete query and ranked result region is visible."""

    deadline = time.monotonic() + timeout
    selector = selectors.DefaultSelector()
    try:
        selector.register(master, selectors.EVENT_READ)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                rows = [screen.line(index) for index in range(5)]
                raise BenchmarkError(
                    f"terminal did not render {expected.query!r} with {expected.count} matches; rows={rows!r}"
                )
            if not selector.select(remaining):
                continue
            try:
                data = os.read(master, 65536)
            except OSError as error:
                raise BenchmarkError(f"terminal closed before redraw: {error}") from error
            if not data:
                raise BenchmarkError("terminal closed before redraw")
            screen.feed(data)
            if screen.matches(expected):
                return
    finally:
        selector.close()


def live_rss_bytes(pid: int) -> Optional[int]:
    """Read current resident memory after the final interactive redraw."""

    if sys.platform.startswith("linux"):
        try:
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            return None
        return None
    if sys.platform == "darwin":
        try:
            output = subprocess.check_output(
                ["/bin/ps", "-o", "rss=", "-p", str(pid)], stderr=subprocess.DEVNULL, text=True
            )
            values = output.split()
            return int(values[0]) * 1024 if values else None  # macOS ps reports KiB here.
        except (OSError, subprocess.CalledProcessError, ValueError):
            return None
    return None


def cancel_process(process: subprocess.Popen[bytes], master: int, original_termios: List[object]) -> Sample:
    os.write(master, b"\x03")
    started = time.perf_counter()
    deadline = time.monotonic() + 10.0
    selector = selectors.DefaultSelector()
    try:
        selector.register(master, selectors.EVENT_READ)
        while True:
            try:
                pid, status, usage = os.wait4(process.pid, os.WNOHANG)
            except InterruptedError:
                continue
            if pid:
                sample = sample_from_wait(process, status, usage, started)
                break
            if time.monotonic() >= deadline:
                raise BenchmarkError("interactive cancellation did not exit")
            for _key, _events in selector.select(0.05):
                try:
                    os.read(master, 65536)
                except OSError:
                    pass
    finally:
        selector.close()
    if sample.returncode != 1:
        raise BenchmarkError(f"interactive cancellation exited {sample.returncode}, expected 1")
    if termios.tcgetattr(master) != original_termios:
        raise BenchmarkError("interactive cancellation did not restore PTY termios")
    return sample


def terminate_process(process: subprocess.Popen[bytes]) -> None:
    try:
        os.kill(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    try:
        wait_for_process(process, time.perf_counter())
    except ChildProcessError:
        pass


def interactive_once(
    prepared: PreparedDataset,
    binary: Path,
    mode: str,
    expectations: Dict[Tuple[str, str], TerminalExpectation],
) -> Tuple[Dict[str, float], Optional[int], Sample]:
    """Measure query-to-final-redraw latency, then prove cancellation restores TTY state."""

    master, slave = pty.openpty()
    original_termios = termios.tcgetattr(master)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 120, 0, 0))
    process: Optional[subprocess.Popen[bytes]] = None
    try:
        arguments = [str(binary)]
        if mode == "-j1":
            arguments.append("-j1")
        # Info makes each result count explicit. Three result rows permit a
        # ranked top-three comparison against the precomputed fzy oracle.
        arguments.extend(["-i", "-l3", "--tty", os.ttyname(slave)])
        with open(os.devnull, "wb") as sink:
            process = subprocess.Popen(
                arguments,
                stdin=subprocess.PIPE,
                stdout=sink,
                stderr=sink,
                close_fds=True,
                start_new_session=True,
            )
            assert process.stdin is not None
            with prepared.path.open("rb") as source:
                while True:
                    chunk = source.read(1024 * 1024)
                    if not chunk:
                        break
                    process.stdin.write(chunk)
            process.stdin.close()
            screen = Screen(rows=24, cols=120)
            empty = expectations[(mode, "empty")]
            # Initial blank-query ranking is setup. Completion includes the
            # info and rows, so it cannot be confused with a prompt-only draw.
            wait_for_render(master, screen, empty, 30.0)
            queries = tuple(query for query in prepared.dataset.queries if query.name != "empty")
            latencies: Dict[str, float] = {}
            for query in queries:
                expected = expectations[(mode, query.name)]
                # Reset and fully synchronize the empty result state before
                # each independent sample. Otherwise fzy can echo the next
                # query above the prior query's rows before it searches.
                os.write(master, b"\x15")
                wait_for_render(master, screen, empty, 30.0)
                started = time.perf_counter()
                os.write(master, query.text.encode())
                wait_for_render(master, screen, expected, 30.0)
                latencies[query.name] = time.perf_counter() - started
            rss_after = live_rss_bytes(process.pid)
            cancelled = cancel_process(process, master, original_termios)
            process = None
            return latencies, rss_after, cancelled
    except Exception:
        if process is not None:
            terminate_process(process)
        raise
    finally:
        if slave >= 0:
            os.close(slave)
        os.close(master)


def measure_interactive(
    prepared: PreparedDataset,
    fz: Path,
    fzy: Path,
    warmups: int,
    repetitions: int,
    expectations: Dict[Tuple[str, str], TerminalExpectation],
) -> Tuple[Dict[Tuple[str, str, str], List[float]], Dict[Tuple[str, str], List[Optional[int]]], Dict[Tuple[str, str], List[Sample]]]:
    latencies: Dict[Tuple[str, str, str], List[float]] = {}
    rss_after: Dict[Tuple[str, str], List[Optional[int]]] = {}
    peak_samples: Dict[Tuple[str, str], List[Sample]] = {}
    for mode in ("default", "-j1"):
        for warmup in range(warmups):
            binaries = (("fz", fz), ("fzy", fzy))
            if warmup % 2:
                binaries = tuple(reversed(binaries))
            for _label, binary in binaries:
                interactive_once(prepared, binary, mode, expectations)
        for repetition in range(repetitions):
            binaries = (("fz", fz), ("fzy", fzy))
            if repetition % 2:
                binaries = tuple(reversed(binaries))
            for label, binary in binaries:
                times, rss, cancelled = interactive_once(prepared, binary, mode, expectations)
                for name, value in times.items():
                    latencies.setdefault((mode, label, name), []).append(value)
                rss_after.setdefault((mode, label), []).append(rss)
                peak_samples.setdefault((mode, label), []).append(cancelled)
    return latencies, rss_after, peak_samples


def duration(value: float) -> str:
    return f"{value * 1000:.2f} ms" if value < 1 else f"{value:.3f} s"


def bytes_text(value: Optional[float]) -> str:
    if value is None:
        return "n/a"
    value = float(value)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.2f} {unit}"
        value /= 1024
    raise AssertionError("unreachable")


def ratio(left: float, right: float) -> str:
    return "n/a" if right == 0 else f"{left / right:.2f}x"


def print_table(headers: Sequence[str], rows: Iterable[Sequence[str]]) -> None:
    materialized = [list(row) for row in rows]
    widths = [len(header) for header in headers]
    for row in materialized:
        for index, value in enumerate(row):
            widths[index] = max(widths[index], len(value))
    print("  ".join(header.ljust(widths[index]) for index, header in enumerate(headers)))
    print("  ".join("-" * width for width in widths))
    for row in materialized:
        print("  ".join(value.ljust(widths[index]) for index, value in enumerate(row)))


def noninteractive_report(
    datasets: Sequence[PreparedDataset], measurements: Dict[Tuple[str, str, str, str], List[Sample]]
) -> List[Dict[str, object]]:
    report: List[Dict[str, object]] = []
    rows = []
    for prepared in datasets:
        for query in prepared.dataset.queries:
            for mode in ("default", "-j1"):
                fz = median_sample(measurements[(prepared.dataset.name, query.name, mode, "fz")])
                fzy = median_sample(measurements[(prepared.dataset.name, query.name, mode, "fzy")])
                report.append({"dataset": prepared.dataset.name, "query": query.name, "mode": mode, "fz": fz, "fzy": fzy})
                rows.append((
                    prepared.dataset.name,
                    query.name,
                    mode,
                    f"{duration(fz['wall_seconds'])} / {duration(fz['cpu_seconds'])} / {bytes_text(fz['peak_rss_bytes'])}",
                    f"{duration(fzy['wall_seconds'])} / {duration(fzy['cpu_seconds'])} / {bytes_text(fzy['peak_rss_bytes'])}",
                    f"{ratio(fz['wall_seconds'], fzy['wall_seconds'])} / {ratio(fz['cpu_seconds'], fzy['cpu_seconds'])} / {ratio(fz['peak_rss_bytes'], fzy['peak_rss_bytes'])}",
                ))
    print("\nNon-interactive medians: wall / child user+sys CPU / peak RSS (fz/fzy ratios below 1 favor fz)")
    print_table(("dataset", "query", "mode", "fz", "fzy", "fz/fzy"), rows)
    return report


def interactive_report(
    dataset_name: str,
    latencies: Dict[Tuple[str, str, str], List[float]],
    rss_after: Dict[Tuple[str, str], List[Optional[int]]],
    peak_samples: Dict[Tuple[str, str], List[Sample]],
) -> List[Dict[str, object]]:
    report: List[Dict[str, object]] = []
    latency_rows = []
    memory_rows = []
    for mode in ("default", "-j1"):
        for operation in ("dense", "selective", "no-match", "exact", "longer"):
            fz = statistics.median(latencies[(mode, "fz", operation)])
            fzy = statistics.median(latencies[(mode, "fzy", operation)])
            report.append({"mode": mode, "operation": operation, "fz_redraw_seconds": fz, "fzy_redraw_seconds": fzy})
            latency_rows.append((mode, operation, duration(fz), duration(fzy), ratio(fz, fzy)))
        fz_live = [value for value in rss_after[(mode, "fz")] if value is not None]
        fzy_live = [value for value in rss_after[(mode, "fzy")] if value is not None]
        fz_peak = median_sample(peak_samples[(mode, "fz")])["peak_rss_bytes"]
        fzy_peak = median_sample(peak_samples[(mode, "fzy")])["peak_rss_bytes"]
        live_fz = statistics.median(fz_live) if fz_live else None
        live_fzy = statistics.median(fzy_live) if fzy_live else None
        report.append({"mode": mode, "fz_rss_after_bytes": live_fz, "fzy_rss_after_bytes": live_fzy, "fz_peak_rss_bytes": fz_peak, "fzy_peak_rss_bytes": fzy_peak})
        memory_rows.append((
            mode,
            bytes_text(live_fz),
            bytes_text(live_fzy),
            ratio(float(live_fz), float(live_fzy)) if live_fz is not None and live_fzy is not None else "n/a",
            bytes_text(fz_peak),
            bytes_text(fzy_peak),
            ratio(fz_peak, fzy_peak),
        ))
    print(f"\nInteractive medians on {dataset_name}: typed query to final PTY redraw")
    print_table(("mode", "operation", "fz", "fzy", "fz/fzy"), latency_rows)
    print("\nInteractive memory after the final redraw; peak RSS is wait4's process-lifetime maximum after verified cancellation")
    print_table(("mode", "fz current", "fzy current", "fz/fzy", "fz peak", "fzy peak", "fz/fzy"), memory_rows)
    return report


def parse_positive(value: str) -> int:
    result = int(value)
    if result < 1:
        raise argparse.ArgumentTypeError("must be at least one")
    return result


def parse_nonnegative(value: str) -> int:
    result = int(value)
    if result < 0:
        raise argparse.ArgumentTypeError("must not be negative")
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fz", type=Path, default=DEFAULT_FZ, help="fz release binary")
    parser.add_argument("--fzy", type=Path, default=DEFAULT_FZY, help="upstream fzy binary")
    parser.add_argument("--data-dir", type=Path, default=DEFAULT_DATA_DIR, help="prepared fixture directory")
    parser.add_argument("--warmup", type=parse_nonnegative, default=None, help="unmeasured runs per binary/workload (default 1; smoke 0)")
    parser.add_argument("--repetitions", type=parse_positive, default=None, help="measured processes per binary/workload (default 5)")
    parser.add_argument("--quick", action="store_true", help="retain all workloads but use three measured repetitions")
    parser.add_argument("--smoke", action="store_true", help="tiny one-repetition harness check; never creates 100k fixtures")
    parser.add_argument("--dataset", action="append", choices=("small", "numeric_100k", "paths_100k", "long_boundary"), help="only measure a named non-interactive dataset (repeatable)")
    parser.add_argument("--no-verify", action="store_true", help="skip pre-timing full-output compatibility checks")
    parser.add_argument("--no-interactive", action="store_true", help="skip PTY redraw and post-sequence memory workload")
    parser.add_argument("--interactive-only", action="store_true", help="only run the PTY workload")
    parser.add_argument("--json", type=Path, help="write summarized medians and raw process samples to this file")
    return parser.parse_args()


def main() -> int:
    options = parse_args()
    if options.interactive_only and options.no_interactive:
        raise BenchmarkError("--interactive-only conflicts with --no-interactive")
    fz = validate_binary(options.fz, "--fz")
    fzy = validate_binary(options.fzy, "--fzy")
    repetitions = options.repetitions if options.repetitions is not None else (3 if options.quick else 5)
    warmups = options.warmup if options.warmup is not None else (0 if options.smoke else 1)
    datasets = make_datasets()
    if options.smoke:
        datasets = make_datasets(records=32, small_records=24)
        selected_names = ("small",)
        repetitions = 1
    else:
        selected_names = tuple(options.dataset) if options.dataset else tuple(datasets)
    # Smoke uses a separate child directory even with an explicit --data-dir.
    # That makes a smoke run safe to perform immediately before a normal run.
    data_dir = options.data_dir / "smoke" if options.smoke else options.data_dir
    selected = [] if options.interactive_only else [
        prepare_dataset(data_dir, datasets[name]) for name in selected_names
    ]
    interactive_dataset = None
    if not options.no_interactive:
        interactive_dataset = prepare_dataset(data_dir, datasets["small" if options.smoke else "paths_100k"])
    print(f"fz:  {fz}")
    print(f"fzy: {fzy}")
    print(f"prepared fixtures: {data_dir.resolve()} (outside timed subprocesses)")
    print(f"warmups: {warmups}; repetitions: {repetitions}; modes: default and -j1 are reported separately")

    fixture_items = list(selected)
    if interactive_dataset is not None and all(
        item.dataset.name != interactive_dataset.dataset.name for item in fixture_items
    ):
        fixture_items.append(interactive_dataset)
    output: Dict[str, object] = {
        "schema": 1,
        "warmups": warmups,
        "repetitions": repetitions,
        "fz": str(fz),
        "fzy": str(fzy),
        "fixtures": [{"name": item.dataset.name, "sha256": item.sha256, "bytes": item.path.stat().st_size} for item in fixture_items],
    }
    terminal_expectations = None
    if not options.no_interactive:
        assert interactive_dataset is not None
        print("preparing scored fzy expectations for complete PTY redraws...")
        terminal_expectations = prepare_terminal_expectations(interactive_dataset, fzy)
    if not options.interactive_only:
        if not options.no_verify:
            print("checking byte-for-byte non-interactive output before timing...")
            for prepared in selected:
                for query in prepared.dataset.queries:
                    for mode in ("default", "-j1"):
                        verify_pair(prepared, query, mode, fz, fzy)
        measurements = measure_noninteractive(selected, fz, fzy, warmups, repetitions)
        output["noninteractive"] = noninteractive_report(selected, measurements)
        output["noninteractive_raw"] = {
            "/".join(key): [dataclasses.asdict(sample) for sample in values]
            for key, values in measurements.items()
        }
    if not options.no_interactive:
        assert interactive_dataset is not None and terminal_expectations is not None
        latencies, rss_after, peaks = measure_interactive(
            interactive_dataset, fz, fzy, warmups, repetitions, terminal_expectations
        )
        output["interactive"] = interactive_report(interactive_dataset.dataset.name, latencies, rss_after, peaks)
        output["interactive_raw"] = {
            "latencies": {"/".join(key): values for key, values in latencies.items()},
            "rss_after": {"/".join(key): values for key, values in rss_after.items()},
            "peak_samples": {"/".join(key): [dataclasses.asdict(sample) for sample in values] for key, values in peaks.items()},
        }
    if options.json:
        options.json.parent.mkdir(parents=True, exist_ok=True)
        options.json.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
        print(f"wrote {options.json}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"perf.py: {error}", file=sys.stderr)
        raise SystemExit(2)

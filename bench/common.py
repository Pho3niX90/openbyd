"""Shared helpers for the benchmarks: statistics, result files, the Rust example binaries and
measuring a process from outside.

Nothing here knows about a particular library: ``micro.py``, ``load.py`` and ``mqtt_idle.py``
own their tables of implementations.
"""

from __future__ import annotations

import contextlib
import datetime as dt
import json
import os
import platform
import queue
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"
RESULTS = BENCH / "results"
CORPUS = ROOT / "parity" / "corpus"
EXE = ".exe" if os.name == "nt" else ""

#: Where scratch files (process logs) go; never the system temp directory on a full drive.
SCRATCH = Path(os.environ.get("OPENBYD_BENCH_SCRATCH", ROOT / "target" / "bench-scratch"))


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------


def summarize(samples: list[float]) -> dict[str, float]:
    """Median, minimum, maximum and relative spread ((max - min) / median) of repeated measurements."""
    median = statistics.median(samples)
    low, high = min(samples), max(samples)
    return {
        "median": median,
        "min": low,
        "max": high,
        "spread": (high - low) / median if median else 0.0,
        "n": len(samples),
    }


def percentile(values: list[float], fraction: float) -> float:
    """The ``fraction`` quantile (0 to 1) by nearest rank; 0.0 for no values."""
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def overlap(a: dict[str, float], b: dict[str, float]) -> bool:
    """Whether the min..max ranges of two summaries overlap: then neither is clearly faster."""
    return a["min"] <= b["max"] and b["min"] <= a["max"]


# ---------------------------------------------------------------------------
# Result files
# ---------------------------------------------------------------------------


def _run(cmd: list[str], cwd: Path | None = None) -> str:
    try:
        done = subprocess.run(cmd, cwd=cwd or ROOT, capture_output=True, text=True, timeout=30, check=False)
    except (OSError, subprocess.SubprocessError):
        return ""
    return done.stdout.strip()


def _cpu_model() -> str:
    if os.name == "nt":
        try:
            import winreg

            key = winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
            return str(winreg.QueryValueEx(key, "ProcessorNameString")[0]).strip()
        except OSError:
            pass
    elif sys.platform == "darwin":
        name = _run(["sysctl", "-n", "machdep.cpu.brand_string"])
        if name:
            return name
    else:
        try:
            for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def machine_info() -> dict[str, Any]:
    """CPU model, core counts, memory, OS and the Python and Rust versions."""
    import psutil

    rust = _run(["rustc", "--version"])
    return {
        "cpu": _cpu_model(),
        "cores_logical": os.cpu_count(),
        "cores_physical": psutil.cpu_count(logical=False),
        "memory_gib": round(psutil.virtual_memory().total / 2**30, 1),
        "os": platform.platform(),
        "python": platform.python_version(),
        "python_implementation": platform.python_implementation(),
        "rust": rust or "unknown",
    }


def versions_info() -> dict[str, Any]:
    """The openbyd commit (and whether the tree has uncommitted changes) and the pyBYD commit."""
    from importlib import metadata

    pinned = (ROOT / "parity" / "UPSTREAM_COMMIT").read_text(encoding="utf-8").strip()
    installed = None
    try:
        direct = metadata.distribution("pybyd").read_text("direct_url.json")
        if direct:
            installed = json.loads(direct).get("vcs_info", {}).get("commit_id")
    except metadata.PackageNotFoundError:
        pass
    status = _run(["git", "status", "--porcelain", "--", ".", ":(exclude)bench/results"])
    try:
        openbyd_version = metadata.version("openbyd")
    except metadata.PackageNotFoundError:
        openbyd_version = None
    return {
        "openbyd_commit": _run(["git", "rev-parse", "HEAD"]) or "unknown",
        "openbyd_branch": _run(["git", "rev-parse", "--abbrev-ref", "HEAD"]) or "unknown",
        "openbyd_dirty": bool(status),
        "openbyd_package_version": openbyd_version,
        "pybyd_commit_pinned": pinned,
        "pybyd_commit_installed": installed,
    }


def write_results(name: str, quick: bool, payload: dict[str, Any], *, directory: Path | None = None, unique: bool = False) -> Path:
    """Write ``bench/results/<name>-<timestamp>[-quick].json`` with the date, machine and commits.

    ``directory`` is another place to write it (results that must stay out of the repository).
    """
    results = RESULTS if directory is None else directory
    results.mkdir(parents=True, exist_ok=True)
    now = dt.datetime.now().astimezone()
    document = {
        "benchmark": name,
        "quick": quick,
        "date": now.isoformat(timespec="seconds"),
        "machine": machine_info(),
        "versions": versions_info(),
        **payload,
    }
    stamp = now.strftime("%Y%m%d-%H%M%S-%f" if unique else "%Y%m%d-%H%M%S")
    path = results / f"{name}-{stamp}{'-quick' if quick else ''}.json"
    path.write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8")
    return path


# ---------------------------------------------------------------------------
# Rust example binaries
# ---------------------------------------------------------------------------


def target_dir() -> Path:
    return Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))


def example(name: str) -> Path:
    """Path of a built release example of openbyd-client."""
    return target_dir() / "release" / "examples" / f"{name}{EXE}"


def build_examples() -> None:
    """Build the example binaries the benchmarks run (release, with the ``testing`` feature)."""
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is not on PATH (see AGENTS.md for the Rust environment)")
    cmd = [cargo, "build", "--release", "-p", "openbyd-client", "--features", "testing", "--examples"]
    print("+", " ".join(cmd), flush=True)
    done = subprocess.run(cmd, cwd=ROOT, check=False)
    if done.returncode != 0:
        raise SystemExit("building the example binaries failed")


def require_examples(*names: str, build: bool = True) -> None:
    if build:
        build_examples()
    missing = [name for name in names if not example(name).exists()]
    if missing:
        raise SystemExit(f"missing example binaries {missing}: run without --skip-build")


# ---------------------------------------------------------------------------
# Processes measured from outside
# ---------------------------------------------------------------------------


class Child:
    """A child process that speaks the benchmarks' line protocol on stdin and stdout.

    A reader thread queues its output lines, so waiting for one can time out. Its stderr goes to a
    log file under :data:`SCRATCH`.
    """

    def __init__(
        self, cmd: list[str | Path], label: str, *, env: dict[str, str] | None = None, cwd: Path | None = None,
        log: Path | None = None,
    ) -> None:
        self.label = label
        self.log = log if log is not None else SCRATCH / f"{label.replace(' ', '_').replace('+', '_')}-{os.getpid()}.log"
        self.log.parent.mkdir(parents=True, exist_ok=True)
        self._stderr = self.log.open("wb")
        env = dict(os.environ if env is None else env)
        env.setdefault("PYTHONUNBUFFERED", "1")
        try:
            self.proc = subprocess.Popen(
                [str(part) for part in cmd],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self._stderr,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
                env=env,
                cwd=cwd or ROOT,
            )
        except BaseException:
            self._stderr.close()
            raise
        self.lines: queue.Queue[str | None] = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self) -> None:
        assert self.proc.stdout is not None  # noqa: S101
        for line in self.proc.stdout:
            self.lines.put(line.rstrip("\r\n"))
        self.lines.put(None)

    def send(self, text: str) -> None:
        assert self.proc.stdin is not None  # noqa: S101
        self.proc.stdin.write(text + "\n")
        self.proc.stdin.flush()

    def read(self, timeout: float) -> str:
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError(f"{self.label}: no output for {timeout:.0f} s") from None
        if line is None:
            raise RuntimeError(f"{self.label} exited early (code {self.proc.poll()}); see {self.log}")
        return line

    def expect(self, prefix: str, timeout: float) -> str:
        """The text after ``prefix`` on the next line that starts with it; other lines are skipped."""
        deadline = time.monotonic() + timeout
        while True:
            line = self.read(max(0.1, deadline - time.monotonic()))
            if line.startswith(prefix):
                return line[len(prefix) :].strip()

    def ask(self, command: str, timeout: float = 60) -> str:
        self.send(command)
        return self.read(timeout)

    def finish(self, timeout: float) -> int:
        """Close input and wait for a child whose protocol ends at EOF."""
        self.proc.stdin.close()
        return self.proc.wait(timeout=timeout)

    def reap(self) -> None:
        """Terminate a child if needed and close its pipes and log."""
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait()
        try:
            # After a failed write the pipe still holds the unsent line and closing it fails again.
            with contextlib.suppress(OSError, ValueError):
                self.proc.stdin.close()
            self.reader.join()
            with contextlib.suppress(OSError, ValueError):
                self.proc.stdout.close()
        finally:
            self._stderr.close()

    def close(self) -> None:
        try:
            if self.proc.poll() is None:
                self.send("quit")
                self.proc.wait(timeout=20)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()
        finally:
            if self.proc.poll() is None:
                self.proc.kill()
            self._stderr.close()


class Sampler(threading.Thread):
    """Polls a process's resident memory and thread count and remembers the peaks."""

    def __init__(self, pid: int, interval: float = 0.02) -> None:
        super().__init__(daemon=True)
        import psutil

        self.process = psutil.Process(pid)
        self.interval = interval
        self.peak_rss = 0
        self.peak_threads = 0
        self.samples = 0
        self._halt = threading.Event()

    def run(self) -> None:
        import psutil

        while not self._halt.is_set():
            try:
                self.peak_rss = max(self.peak_rss, self.process.memory_info().rss)
                self.peak_threads = max(self.peak_threads, self.process.num_threads())
                self.samples += 1
            except psutil.Error:
                return
            self._halt.wait(self.interval)

    def stop(self) -> None:
        self._halt.set()
        self.join(timeout=5)


def os_peak_rss(process: Any) -> int | None:
    """The operating system's own peak resident size of a live process, when it keeps one."""
    try:
        info = process.memory_info()
    except Exception:  # noqa: BLE001 - psutil.Error and platform quirks
        return None
    peak = getattr(info, "peak_wset", None)  # Windows
    if peak:
        return int(peak)
    try:  # Linux
        for line in Path(f"/proc/{process.pid}/status").read_text(encoding="ascii").splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1]) * 1024
    except OSError:
        pass
    return None


def process_snapshot(process: Any) -> dict[str, float]:
    times = process.cpu_times()
    return {
        "cpu_s": times.user + times.system,
        "cpu_user_s": times.user,
        "cpu_system_s": times.system,
        "rss": process.memory_info().rss,
        "threads": process.num_threads(),
    }


def background_cpu(seconds: float = 2.0) -> float:
    """System-wide CPU utilisation (percent of all cores) over ``seconds``: what else the machine is doing."""
    import psutil

    return psutil.cpu_percent(interval=seconds)


def wait_for_quiet_ports(max_time_wait: int = 1000, timeout: float = 300.0) -> int:
    """Wait until few TCP sockets are in TIME_WAIT; returns how many there were when it stopped waiting.

    After a load run thousands of loopback sockets linger for a couple of minutes, and a benchmark that
    opens connections right after (paho opens a socket pair per client on Windows) can fail with
    "address already in use" for reasons that have nothing to do with the library.
    """
    import psutil

    deadline = time.monotonic() + timeout
    while True:
        waiting = sum(1 for conn in psutil.net_connections("tcp") if conn.status == "TIME_WAIT")
        if waiting <= max_time_wait or time.monotonic() > deadline:
            return waiting
        time.sleep(5)


def mib(value: float) -> float:
    return value / 2**20


class FakeServerProcess:
    """The ``fake_server`` example binary: a fake BYD server in a process of its own."""

    def __init__(self, accounts: int, delay_ms: int) -> None:
        self.child = Child(
            [example("fake_server"), "--accounts", str(accounts), "--delay-ms", str(delay_ms)],
            "fake_server",
        )
        self.url = self.child.expect("URL", 60)
        self.child.expect("READY", 60)

    def stats(self) -> dict[str, Any]:
        return json.loads(self.child.ask("stats", 120))

    def reset(self) -> None:
        self.child.ask("reset")

    def close(self) -> None:
        self.child.close()

    def __enter__(self) -> FakeServerProcess:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


def measured_process(pid: int) -> Any:
    """The process to measure for a child started as ``pid``.

    A virtual environment's ``python.exe`` on Windows is a small launcher that starts the real
    interpreter as its child and waits for it; the work happens in the child. Elsewhere, and for
    the Rust binaries, it is the process itself.
    """
    import psutil

    process = psutil.Process(pid)
    if os.name == "nt":
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            children = [child for child in process.children() if child.name().lower().startswith("python")]
            if children:
                return children[0]
            if process.name().lower().startswith("python") is False:
                break
            time.sleep(0.05)
    return process


def measure_child(cmd: list[str | Path], label: str, *, ready_timeout: float = 120, run_timeout: float = 4 * 3600) -> dict[str, Any]:
    """Run a worker that prints ``READY``, waits for ``go``, prints ``RESULT <json>`` and waits for
    ``quit``, and measure it from outside.

    Returns what the worker reported (``reported``) next to what the operating system says about
    the process (``measured``): wall time of the run, CPU seconds used by the run (user plus
    system, after ``READY``), resident memory before the run, at the end and at its peak, and the
    most threads seen.
    """
    import psutil

    child = Child(cmd, label)
    try:
        child.expect("READY", ready_timeout)
        process = measured_process(child.proc.pid)
        before = process_snapshot(process)
        sampler = Sampler(process.pid)
        sampler.start()
        psutil.cpu_percent(interval=None)
        started = time.perf_counter()
        child.send("go")
        reported = json.loads(child.expect("RESULT", run_timeout))
        wall = time.perf_counter() - started
        system_cpu = psutil.cpu_percent(interval=None)
        after = process_snapshot(process)
        peak = os_peak_rss(process)
        sampler.stop()
        peak_rss = max(sampler.peak_rss, peak or 0, after["rss"])
        return {
            "reported": reported,
            "measured": {
                "wall_s": wall,
                "cpu_s": after["cpu_s"] - before["cpu_s"],
                "cpu_user_s": after["cpu_user_s"] - before["cpu_user_s"],
                "cpu_system_s": after["cpu_system_s"] - before["cpu_system_s"],
                "rss_before_mib": mib(before["rss"]),
                "rss_after_mib": mib(after["rss"]),
                "rss_peak_mib": mib(peak_rss),
                "rss_peak_source": "os" if peak else "sampled",
                "threads_before": before["threads"],
                "threads_peak": max(sampler.peak_threads, after["threads"]),
                "samples": sampler.samples,
                "system_cpu_percent": system_cpu,
            },
        }
    finally:
        child.close()

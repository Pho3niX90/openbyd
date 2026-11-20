"""Run a Python module (the backend's mqtt-worker) with a loopback-only network guard.

    python bench/replay/launcher.py [--guard-report FILE] [--stdin-stop] [--speed N] -m app.mqtt_worker [args...]

``bench/replay_worker.py`` starts the backend's real worker through this launcher, from the
backend checkout, and measures this process from outside. The launcher:

1. installs the guard before anything else is imported: resolving or connecting to any address
   that is not loopback fails at once (``socket.gaierror`` for a lookup, ``ConnectionRefusedError``
   for a connect), and every attempt is counted. Covered: ``socket.getaddrinfo``,
   ``gethostbyname(_ex)``, ``gethostbyaddr``, ``getnameinfo``, ``socket.socket.connect``,
   ``connect_ex`` and ``sendto``, and on Windows the proactor event loop's overlapped
   ``IocpProactor.connect`` and ``sendto``, which do not go through ``socket.connect``. ``aiodns``
   and ``pycares`` (their own DNS sockets) are made unimportable, so aiohttp uses the guarded
   threaded resolver. Whatever the worker is configured to do, it cannot reach BYD, Sentry, a
   geocoder, a push service or anything else off this machine;
2. runs the module as ``__main__`` (``runpy``), unchanged: nothing of the worker is replaced;
3. with ``--stdin-stop``: on the standard-input line ``stop``, or when standard input closes,
   raises SIGINT in the process, which the worker handles as a clean shutdown on every platform
   (on Windows there is no SIGTERM to send it from outside);
4. prints ``GUARD_REPORT <json>`` on standard output when the module returns (and writes the
   same JSON to ``--guard-report``): how many attempts were blocked, and which host, port and
   path each was.

``--speed N`` (N > 1) is experimental: see :func:`install_speed`.

Standard output carries only the launcher's lines (``LAUNCHER ...``, ``GUARD_REPORT ...``); the
worker logs to standard error.
"""

from __future__ import annotations

import errno
import ipaddress
import json
import os
import signal
import socket
import sys
import threading
from typing import Any

#: Prefix of every refusal message, so a caller can tell a guard refusal from a real failure.
REFUSAL = "replay guard"

_LOCAL_NAMES = {"localhost", "localhost.", "ip6-localhost", "ip6-loopback"}


def _host_text(host: Any) -> str:
    if isinstance(host, (bytes, bytearray)):
        return bytes(host).decode("ascii", "replace")
    return "" if host is None else str(host)


def _ip(host: str) -> ipaddress.IPv4Address | ipaddress.IPv6Address | None:
    text = host.strip().strip("[]").split("%", 1)[0]
    try:
        return ipaddress.ip_address(text)
    except ValueError:
        return None


def address_is_local(host: Any) -> bool:
    """Whether connecting to ``host`` stays on this machine: a loopback or unspecified IP literal,
    an IPv4-mapped form of one, or ``localhost``. Any other name is refused, because a name would
    be resolved by C code that the guard cannot see."""
    text = _host_text(host)
    if text.lower() in _LOCAL_NAMES:
        return True
    ip = _ip(text)
    if ip is None:
        return False
    mapped = getattr(ip, "ipv4_mapped", None)
    if mapped is not None:
        ip = mapped
    return ip.is_loopback or ip.is_unspecified


def lookup_is_local(host: Any) -> bool:
    """Whether a name lookup may go ahead: no host (a passive bind), ``localhost`` or a local IP."""
    text = _host_text(host)
    return text == "" or address_is_local(text)


class Guard:
    """The installed patches and the attempts they refused."""

    def __init__(self) -> None:
        self.blocked: dict[tuple[str, str, str], int] = {}
        self._lock = threading.Lock()
        self.installed: list[str] = []

    def record(self, via: str, host: Any, port: Any) -> None:
        key = (via, _host_text(host), "" if port is None else str(port))
        with self._lock:
            self.blocked[key] = self.blocked.get(key, 0) + 1
        print(f"{REFUSAL}: blocked {via} {key[1]}:{key[2]}", file=sys.stderr, flush=True)

    def report(self) -> dict[str, Any]:
        with self._lock:
            items = sorted(self.blocked.items())
        return {
            "installed": list(self.installed),
            "blocked_total": sum(count for _key, count in items),
            "blocked": [{"via": via, "host": host, "port": port, "count": count} for (via, host, port), count in items],
        }

    def refuse_lookup(self, via: str, host: Any, port: Any = None) -> socket.gaierror:
        self.record(via, host, port)
        return socket.gaierror(socket.EAI_NONAME, f"{REFUSAL}: {_host_text(host)} is not a loopback address")

    def refuse_connect(self, via: str, host: Any, port: Any) -> ConnectionRefusedError:
        self.record(via, host, port)
        return ConnectionRefusedError(errno.ECONNREFUSED, f"{REFUSAL}: {via} to {_host_text(host)}:{port} refused")


GUARD: Guard | None = None


def _inet(sock: Any) -> bool:
    return getattr(sock, "family", None) in (socket.AF_INET, socket.AF_INET6)


def _split(address: Any) -> tuple[Any, Any]:
    if isinstance(address, tuple) and address:
        return address[0], address[1] if len(address) > 1 else None
    return address, None


def install_guard() -> Guard:
    """Patch the socket layer so only loopback addresses can be resolved or reached. Idempotent."""
    global GUARD
    if GUARD is not None:
        return GUARD
    guard = Guard()

    # Libraries that do DNS on sockets of their own would bypass the patched lookups.
    for name in ("aiodns", "pycares"):
        if name not in sys.modules:
            sys.modules[name] = None  # type: ignore[assignment]
    guard.installed.append("aiodns/pycares disabled")

    original_getaddrinfo = socket.getaddrinfo

    def getaddrinfo(host: Any, port: Any, *args: Any, **kwargs: Any) -> list[Any]:
        if not lookup_is_local(host):
            raise guard.refuse_lookup("getaddrinfo", host, port)
        results = original_getaddrinfo(host, port, *args, **kwargs)
        kept = [entry for entry in results if address_is_local(entry[4][0])]
        if results and not kept:
            raise guard.refuse_lookup("getaddrinfo-result", host, port)
        return kept

    original_gethostbyname = socket.gethostbyname
    original_gethostbyname_ex = socket.gethostbyname_ex
    original_gethostbyaddr = socket.gethostbyaddr
    original_getnameinfo = socket.getnameinfo

    def gethostbyname(host: Any) -> str:
        if not lookup_is_local(host):
            raise guard.refuse_lookup("gethostbyname", host)
        return original_gethostbyname(host)

    def gethostbyname_ex(host: Any) -> Any:
        if not lookup_is_local(host):
            raise guard.refuse_lookup("gethostbyname_ex", host)
        return original_gethostbyname_ex(host)

    def gethostbyaddr(host: Any) -> Any:
        if not address_is_local(host):
            raise guard.refuse_lookup("gethostbyaddr", host)
        return original_gethostbyaddr(host)

    def getnameinfo(sockaddr: Any, flags: int) -> Any:
        host, port = _split(sockaddr)
        if not address_is_local(host):
            raise guard.refuse_lookup("getnameinfo", host, port)
        return original_getnameinfo(sockaddr, flags)

    socket.getaddrinfo = getaddrinfo  # type: ignore[assignment]
    socket.gethostbyname = gethostbyname  # type: ignore[assignment]
    socket.gethostbyname_ex = gethostbyname_ex  # type: ignore[assignment]
    socket.gethostbyaddr = gethostbyaddr  # type: ignore[assignment]
    socket.getnameinfo = getnameinfo  # type: ignore[assignment]
    guard.installed.append("socket lookups")

    original_connect = socket.socket.connect
    original_connect_ex = socket.socket.connect_ex
    original_sendto = socket.socket.sendto

    def connect(self: socket.socket, address: Any) -> None:
        if _inet(self):
            host, port = _split(address)
            if not address_is_local(host):
                raise guard.refuse_connect("socket.connect", host, port)
        return original_connect(self, address)

    def connect_ex(self: socket.socket, address: Any) -> int:
        if _inet(self):
            host, port = _split(address)
            if not address_is_local(host):
                guard.record("socket.connect_ex", host, port)
                return errno.ECONNREFUSED
        return original_connect_ex(self, address)

    def sendto(self: socket.socket, data: Any, *args: Any) -> int:
        address = args[-1] if args else None
        if _inet(self) and address is not None:
            host, port = _split(address)
            if not address_is_local(host):
                raise guard.refuse_connect("socket.sendto", host, port)
        return original_sendto(self, data, *args)

    socket.socket.connect = connect  # type: ignore[method-assign]
    socket.socket.connect_ex = connect_ex  # type: ignore[method-assign]
    socket.socket.sendto = sendto  # type: ignore[method-assign]
    guard.installed.append("socket.connect/connect_ex/sendto")

    if sys.platform == "win32":
        # The proactor loop connects with overlapped ConnectEx on the raw handle.
        from asyncio import windows_events

        proactor = windows_events.IocpProactor
        original_proactor_connect = proactor.connect
        original_proactor_sendto = proactor.sendto

        def proactor_connect(self: Any, conn: Any, address: Any) -> Any:
            if _inet(conn):
                host, port = _split(address)
                if not address_is_local(host):
                    raise guard.refuse_connect("proactor.connect", host, port)
            return original_proactor_connect(self, conn, address)

        def proactor_sendto(self: Any, conn: Any, buf: Any, flags: int = 0, addr: Any = None) -> Any:
            if _inet(conn) and addr is not None:
                host, port = _split(addr)
                if not address_is_local(host):
                    raise guard.refuse_connect("proactor.sendto", host, port)
            return original_proactor_sendto(self, conn, buf, flags, addr)

        proactor.connect = proactor_connect  # type: ignore[method-assign]
        proactor.sendto = proactor_sendto  # type: ignore[method-assign]
        guard.installed.append("IocpProactor.connect/sendto")

    GUARD = guard
    return guard


# ---------------------------------------------------------------------------
# Experimental: replay speed
# ---------------------------------------------------------------------------


def install_speed(speed: float, wall_origin: float, monotonic_origin: float) -> None:
    """Not implemented: scaling the worker's clocks for replay speeds above 1 is left out.

    Scaling ``time.time``/``monotonic`` and ``datetime.now`` is easy; what made it not robust
    enough is the rest: asyncio's timer waits, ``asyncio.wait_for`` and lease TTLs in Redis or
    Postgres (``NOW()``) run on clocks the process does not own, so cadences would hold for some
    paths and not others. Run replays at speed 1 with ``--epoch now``.
    """
    raise SystemExit("--speed above 1 is not supported by this launcher; run the replay at speed 1")


# ---------------------------------------------------------------------------
# Running the module
# ---------------------------------------------------------------------------


def _watch_stdin() -> None:
    """``stop`` or end of input: ask the module to stop as Ctrl+C would."""
    for line in sys.stdin:
        if line.strip() == "stop":
            break
    print("LAUNCHER stopping", flush=True)
    signal.raise_signal(signal.SIGINT)


def _parse(argv: list[str]) -> tuple[str | None, bool, float, str, list[str]]:
    report: str | None = None
    stdin_stop = False
    speed = 1.0
    index = 0
    while index < len(argv):
        arg = argv[index]
        if arg == "--guard-report":
            report = argv[index + 1]
            index += 2
        elif arg == "--stdin-stop":
            stdin_stop = True
            index += 1
        elif arg == "--speed":
            speed = float(argv[index + 1])
            index += 2
        elif arg == "-m":
            return report, stdin_stop, speed, argv[index + 1], argv[index + 2 :]
        else:
            raise SystemExit(
                f"launcher: unexpected argument {arg!r}; usage: launcher.py [--guard-report F] [--stdin-stop] [--speed N] -m MODULE [args]"
            )
    raise SystemExit("launcher: -m MODULE is required")


def main(argv: list[str] | None = None) -> int:
    report_path, stdin_stop, speed, module, rest = _parse(list(sys.argv[1:] if argv is None else argv))
    guard = install_guard()
    if speed > 1:
        install_speed(speed, 0.0, 0.0)
    # Import the module from the working directory (the backend checkout), never from bench/replay.
    here = os.path.dirname(os.path.abspath(__file__))
    sys.path[:] = [p for p in sys.path if os.path.abspath(p or ".") != here]
    sys.path.insert(0, os.getcwd())
    print(f"LAUNCHER pid={os.getpid()} module={module} guard={','.join(guard.installed)}", flush=True)
    if stdin_stop:
        threading.Thread(target=_watch_stdin, name="launcher-stdin", daemon=True).start()
    import runpy

    code = 0
    sys.argv = [module, *rest]
    try:
        runpy.run_module(module, run_name="__main__", alter_sys=True)
    except KeyboardInterrupt:
        code = 130
    except SystemExit as exc:
        code = exc.code if isinstance(exc.code, int) else (0 if exc.code is None else 1)
    finally:
        report = guard.report()
        report["exit_code"] = code
        if report_path:
            with open(report_path, "w", encoding="utf-8") as handle:
                json.dump(report, handle)
        print("GUARD_REPORT " + json.dumps(report), flush=True)
    return code


if __name__ == "__main__":
    raise SystemExit(main())

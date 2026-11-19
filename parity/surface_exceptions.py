"""pyBYD symbols that openbyd deliberately does not port.

Maps a parity call name, as listed by ``parity.surface.expected_calls()``, to
the reason it is not ported. A call listed here counts as covered by
``python/tests/test_surface.py``. The same test fails if an entry names a
symbol that pyBYD no longer has, or if the call now has an oracle, so stale
entries get noticed.

Keep this empty unless a symbol is out of scope on purpose, and say why.
"""

NOT_PORTED: dict[str, str] = {}

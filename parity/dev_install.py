"""Build the Python extension and install it into the current environment.

    python parity/dev_install.py

Use this instead of ``maturin develop``. ``develop`` copies the compiled
module into ``python/openbyd/``, where a second checkout user (or a running
Python on Windows) would collide with it. This builds a wheel into the
cargo target directory and installs that.

Honours ``CARGO_TARGET_DIR``, so several people can build side by side.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    wheels = target / "dev-wheels"
    shutil.rmtree(wheels, ignore_errors=True)

    maturin = shutil.which("maturin") or str(Path(sys.executable).with_name("maturin"))
    build = subprocess.run(
        [maturin, "build", "--release", "--interpreter", sys.executable, "--out", str(wheels)],
        cwd=ROOT,
    )
    if build.returncode != 0:
        return build.returncode
    (wheel,) = wheels.glob("openbyd-*.whl")

    uv = shutil.which("uv")
    if uv:
        install = [uv, "pip", "install", "--python", sys.executable, "--reinstall", "--no-deps", str(wheel)]
    else:
        install = [sys.executable, "-m", "pip", "install", "--force-reinstall", "--no-deps", str(wheel)]
    return subprocess.run(install, cwd=ROOT).returncode


if __name__ == "__main__":
    raise SystemExit(main())

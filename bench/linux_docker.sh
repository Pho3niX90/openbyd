#!/usr/bin/env bash
# In-container half of bench/linux_docker.ps1: build openbyd on Linux and time
# VehicleRealtimeData parsing against pyBYD. Not meant to be run by hand; the
# PowerShell script starts a python:3.12-slim container and calls this with a phase.
#
#   linux_docker.sh <phase>
#     setup      apt packages, Rust toolchain, virtualenv, pyBYD at parity/UPSTREAM_COMMIT
#     build      copy the sources, build both wheels (mimalloc on and off), install the default one
#     test       pytest python/tests/test_corpus.py
#     measure    level 1 (cargo bench x3) and level 2 (Python API) on the corpus and on $RT_CORPUS
#     allocator  headline row, mimalloc and glibc wheels alternating, three rounds
#     info       machine, toolchain and package versions
#
# Mounts: /src (the checkout, read-only), /work (scratch: caches, target dir, virtualenv,
# the build copy of the sources), optionally $RT_CORPUS (a VehicleRealtimeData.json, read-only).
set -euo pipefail

WORK=${WORK:-/work}
SRC=${SRC:-/src}
BUILD=$WORK/src
RUSTC_VERSION=${RUSTC_VERSION:-1.100.0}
RUNS=${RUNS:-7}
ITERATIONS=${ITERATIONS:-20000}
export RUSTUP_HOME=$WORK/rustup CARGO_HOME=$WORK/cargo CARGO_TARGET_DIR=$WORK/target
export PIP_CACHE_DIR=$WORK/pip-cache PIP_DISABLE_PIP_VERSION_CHECK=1 PYTHONDONTWRITEBYTECODE=1
export PATH=$WORK/venv/bin:$CARGO_HOME/bin:$PATH
export CARGO_TERM_COLOR=never

phase=${1:?usage: linux_docker.sh <setup|build|test|measure|allocator|info>}

# Every phase runs in a fresh container, so the system packages are installed in each.
packages() {
  if ! command -v gcc >/dev/null || ! command -v git >/dev/null; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
      build-essential git curl ca-certificates pkg-config util-linux procps >/dev/null
  fi
}

setup() {
  if ! [ -x "$CARGO_HOME/bin/rustc" ] || ! rustc -V | grep -q "^rustc $RUSTC_VERSION "; then
    curl -fsSL https://sh.rustup.rs -o "$WORK/rustup-init.sh"
    sh "$WORK/rustup-init.sh" -y --profile minimal --default-toolchain "$RUSTC_VERSION" --no-modify-path
  fi
  rustc -V
  if ! [ -x "$WORK/venv/bin/python" ]; then python -m venv "$WORK/venv"; fi
  commit=$(cat "$SRC/parity/UPSTREAM_COMMIT")
  repo=$(cat "$SRC/parity/UPSTREAM_REPO")
  pip install -q maturin pytest psutil "pybyd @ git+$repo@$commit"
}

sources() {
  rm -rf "$BUILD"
  mkdir -p "$BUILD"
  # A writable copy: pytest and the benchmarks write next to the sources. Times are kept,
  # so cargo's fingerprints stay valid between runs.
  tar -C "$SRC" --exclude=.git --exclude=target --exclude=.venv\* --exclude=__pycache__ -cf - . | tar -C "$BUILD" -xf -
}

build() {
  sources
  cd "$BUILD"
  mkdir -p "$WORK/wheels/default" "$WORK/wheels/nomimalloc"
  rm -f "$WORK"/wheels/*/*.whl
  # Same command as parity/dev_install.py: release, feature `testing`, default features (mimalloc).
  maturin build --release --features testing --interpreter "$(which python)" --out "$WORK/wheels/default"
  # The comparison build: the system (glibc) allocator.
  maturin build --release --no-default-features --features testing --interpreter "$(which python)" --out "$WORK/wheels/nomimalloc"
  install_wheel default
  for kind in default nomimalloc; do
    wheel=$(ls "$WORK"/wheels/$kind/openbyd-*.whl)
    python - "$wheel" "$WORK/native-$kind.so" <<'PY'
import sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
name = next(n for n in z.namelist() if "_native" in n and n.endswith(".so"))
open(sys.argv[2], "wb").write(z.read(name))
PY
    echo "$kind wheel: $(basename "$wheel"), mimalloc strings in .so: $(grep -c -a -i mimalloc "$WORK/native-$kind.so" || true)"
  done
}

install_wheel() { # default | nomimalloc
  pip install -q --force-reinstall --no-deps "$WORK"/wheels/$1/openbyd-*.whl
}

test_corpus() {
  cd "$BUILD"
  pytest python/tests/test_corpus.py -q -p no:cacheprovider
}

info() {
  echo "date:      $(date -Is)"
  echo "kernel:    $(uname -srm)"
  echo "os:        $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "glibc:     $(ldd --version | head -1)"
  echo "python:    $(python -V) ($(python -c 'import sys; print(sys.executable)'))"
  echo "rustc:     $(rustc -V)"
  echo "cargo:     $(cargo -V)"
  echo "cpu:       $(lscpu | sed -n 's/^Model name: *//p')"
  echo "cpus:      $(nproc) visible; $(lscpu | sed -n 's/^Thread(s) per core: *//p') thread(s)/core; max MHz $(lscpu | sed -n 's/^CPU max MHz: *//p')"
  echo "memory:    $(free -m | awk '/Mem:/ {print $2 " MiB"}')"
  echo "pybyd:     $(pip show pybyd 2>/dev/null | sed -n 's/^Version: //p') at $(cat "$SRC/parity/UPSTREAM_COMMIT" | cut -c1-10); pydantic $(pip show pydantic | sed -n 's/^Version: //p'), pydantic-core $(pip show pydantic-core | sed -n 's/^Version: //p')"
  echo "openbyd:   $(pip show openbyd 2>/dev/null | sed -n 's/^Version: //p')"
  echo "load:      $(cat /proc/loadavg)"
}

# One table: openbyd.models.parse against pyBYD model_validate().model_dump(mode="json"),
# interleaved, RUNS runs of ITERATIONS calls, per payload of a VehicleRealtimeData.json.
levels() { # <corpus file> <label> <redact: 0|1>
  cd "$BUILD"
  python - "$1" "$2" "$3" "$RUNS" "$ITERATIONS" <<'PY'
import json, statistics, sys, time
sys.path.insert(0, "crates/openbyd-protocol/benches")
import compare_pybyd as c
from openbyd import models

path, label, redact, runs, iterations = sys.argv[1], sys.argv[2], sys.argv[3] == "1", int(sys.argv[4]), int(sys.argv[5])
entries = json.loads(open(path, encoding="utf-8").read())
cls = c.pybyd_class("VehicleRealtimeData")
print(f"== {label}: {len(entries)} payloads, {runs} runs x {iterations} calls (x2 for payloads under 1 KB)")
print(f"{'#':>3} {'note':<26} {'fields':>6} {'bytes':>6} | {'openbyd us (med  min  max)':<28} | {'pyBYD us (med  min  max)':<28} | {'ratio med':>9} {'min/min':>8} {'same':>5}")
for i, e in enumerate(entries):
    p = e["payload"]
    expected = cls.model_validate(p).model_dump(mode="json")
    same = models.parse("VehicleRealtimeData", p) == expected
    ours = lambda p=p: models.parse("VehicleRealtimeData", p)
    theirs = lambda p=p: cls.model_validate(p).model_dump(mode="json")
    # Tiny payloads get twice the calls so a run is not over in a few milliseconds.
    n = iterations if len(json.dumps(p)) > 1000 else iterations * 2
    c.per_call(ours, n // 10); c.per_call(theirs, n // 10)
    o, t = [], []
    for _ in range(runs):
        o.append(c.per_call(ours, n) * 1e6)
        t.append(c.per_call(theirs, n) * 1e6)
    note = f"{e['note'][:26]}" if not redact else ("every field present" if e["note"] == "every field present" else "real reply")
    print(f"{i:>3} {note:<26} {len(p):>6} {len(json.dumps(p)):>6} | "
          f"{statistics.median(o):>8.2f} {min(o):>8.2f} {max(o):>8.2f} | {statistics.median(t):>8.2f} {min(t):>8.2f} {max(t):>8.2f} | "
          f"{statistics.median(t) / statistics.median(o):>8.2f}x {min(t) / min(o):>7.2f}x {'yes' if same else 'NO':>5}")
PY
}

measure() {
  cd "$BUILD"
  echo "### host state: $(cat /proc/loadavg)"
  for i in 1 2 3; do
    echo "### level 1, run $i (cargo bench -p openbyd-protocol --bench protocol -- realtime)"
    cargo bench -p openbyd-protocol --bench protocol -- realtime 2>&1 | grep -v -E '^\s*(Compiling|Finished|Running|Downloaded|Downloading)'
  done
  echo "### level 2: compare_pybyd.py VehicleRealtimeData --runs $RUNS"
  python crates/openbyd-protocol/benches/compare_pybyd.py VehicleRealtimeData --runs "$RUNS"
  echo "### level 2, min/max table"
  levels "$BUILD/parity/corpus/VehicleRealtimeData.json" "parity/corpus" 0
  echo "### bench/micro.py, reply-model rows"
  python bench/micro.py --only "Reply models" 2>&1 | grep -v -i -E 'request|load|mqtt' || true
  if [ -f "$BUILD/bench/realtime_levels.py" ]; then
    echo "### bench/realtime_levels.py"
    mkdir -p "$WORK/json"
    extra=()
    if [ -n "${RT_CORPUS:-}" ] && [ -f "$RT_CORPUS" ]; then extra=(--corpus "$RT_CORPUS"); fi
    python bench/realtime_levels.py --json "$WORK/json/realtime_levels-$(date +%Y%m%d-%H%M%S).json" "${extra[@]}" \
      || echo "realtime_levels.py failed: $?"
  fi
  if [ -n "${RT_CORPUS:-}" ] && [ -f "$RT_CORPUS" ]; then
    echo "### level 4: real replies from more cars ($RT_CORPUS), by index only"
    levels "$RT_CORPUS" "corpus-rt" 1
  fi
  echo "### host state after: $(cat /proc/loadavg)"
}

# The headline row for the installed wheel: the "every field present" payload, RUNS runs.
headline() { # <label>
  python - "$1" "$BUILD/parity/corpus/VehicleRealtimeData.json" "$RUNS" "$ITERATIONS" <<'PY'
import json, statistics, sys
sys.path.insert(0, "crates/openbyd-protocol/benches")
import compare_pybyd as c
from openbyd import models
label, runs, n = sys.argv[1], int(sys.argv[3]), int(sys.argv[4])
entries = json.loads(open(sys.argv[2], encoding="utf-8").read())
p = next(e["payload"] for e in entries if e["note"] == "every field present")
cls = c.pybyd_class("VehicleRealtimeData")
ours = lambda: models.parse("VehicleRealtimeData", p)
theirs = lambda: cls.model_validate(p).model_dump(mode="json")
c.per_call(ours, n // 10); c.per_call(theirs, n // 10)
o, t = [], []
for _ in range(runs):
    o.append(c.per_call(ours, n) * 1e6); t.append(c.per_call(theirs, n) * 1e6)
print(f"{label:<11} openbyd us med {statistics.median(o):6.2f} min {min(o):6.2f} max {max(o):6.2f} | pyBYD us med {statistics.median(t):6.2f} min {min(t):6.2f} max {max(t):6.2f} | ratio {statistics.median(t)/statistics.median(o):.2f}x")
PY
}

# Headline row only, alternating the two wheels (default = mimalloc, nomimalloc = glibc) so
# slow drifts of the machine hit both alike.
allocator() {
  cd "$BUILD"
  for round in 1 2 3; do
    for kind in default nomimalloc; do
      install_wheel $kind
      headline $kind
    done
  done
  install_wheel default
  echo "### default wheel (mimalloc) restored"
}

packages
case "$phase" in
  setup) setup ;;
  build) build ;;
  test) test_corpus ;;
  measure) measure ;;
  allocator) allocator ;;
  info) info ;;
  *) echo "unknown phase $phase" >&2; exit 2 ;;
esac

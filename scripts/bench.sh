#!/usr/bin/env bash
# moreland end-to-end performance benchmark.
#
# Usage:
#   scripts/bench.sh                 run once, compare against baseline
#   scripts/bench.sh --save          run once, overwrite the baseline
#   scripts/bench.sh --runs N        run N times, median of medians
#   scripts/bench.sh --seconds N     session duration per run [default: 30]
#   scripts/bench.sh --threshold X   fail if median regresses by more than
#                                    X (default: 1.10, i.e. 10%)
#
# Environment:
#   MORELAND_BENCH_MIN_FRAMES   minimum frames per session for a run to be
#                               considered representative [default: 60]
#
# The baseline is machine- and workload-specific. Regenerate it whenever the
# encoder backend, GPU, or reference workload changes. Do not commit a
# baseline from someone else's laptop.
#
# The virtual output must have continuous damage or the numbers describe the
# idle path (~1 fps, meaningless latencies). Have something animating on it —
# a terminal running a spinner, a video, `while true; do date; done` —
# before running. The frame-count guard below will refuse to produce a
# baseline from an idle run.
#
# The binary used is ~/.local/bin/moreland, not target/release/moreland.
# KWin only grants the screencast interface to the executable path that the
# .desktop entry names, and install.sh copies (does not symlink) to
# ~/.local/bin. Running target/ directly makes KWin deny the interface
# silently and no session ever produces a report.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HOME/.local/bin/moreland"
BASELINE="$REPO/testdata/bench/baseline.json"

SECONDS_RUN=30
RUNS=1
SAVE=0
THRESHOLD=1.10
MIN_FRAMES_PER_SESSION="${MORELAND_BENCH_MIN_FRAMES:-60}"
export MORELAND_BENCH_MIN_FRAMES="$MIN_FRAMES_PER_SESSION"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --save)      SAVE=1; shift ;;
        --runs)      RUNS="$2"; shift 2 ;;
        --seconds)   SECONDS_RUN="$2"; shift 2 ;;
        --threshold) THRESHOLD="$2"; shift 2 ;;
        -h|--help)   sed -n '2,25p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *)           echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

[[ -x "$BIN" ]] || { echo "error: $BIN not found; run ./install.sh" >&2; exit 1; }
mkdir -p "$(dirname "$BASELINE")"

merge_runs() {
    # stdin: concatenated JSONL from N runs. Prints one object on stdout.
    # Exits non-zero if any session produced too few frames, so a bench
    # against an idle virtual output fails loudly instead of writing a bad
    # baseline.
    python3 -c '
import sys, json, statistics, os
min_frames = int(os.environ.get("MORELAND_BENCH_MIN_FRAMES", "60"))

sessions = []
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        sessions.append(json.loads(line))
    except json.JSONDecodeError:
        pass

if not sessions:
    sys.exit("no reports produced")

low = [s for s in sessions if s.get("frames_captured", 0) < min_frames]
if low:
    for s in low:
        n = s.get("frames_captured", 0)
        print(f"error: session produced {n} frames, below the {min_frames}-frame minimum. "
              f"The virtual output was probably idle.", file=sys.stderr)
    print("hint: put something animating on the virtual output (a terminal "
          "running `while true; do date; done`, or a looping video) before "
          "running the benchmark.", file=sys.stderr)
    sys.exit(2)

med = [s["round_trip_ms"]["median"] for s in sessions if s.get("round_trip_ms")]
p95 = [s["round_trip_ms"]["p95"] for s in sessions if s.get("round_trip_ms")]

print(json.dumps({
    "sessions": len(sessions),
    "frames_captured": sum(s["frames_captured"] for s in sessions),
    "bytes_sent": sum(s["bytes_sent"] for s in sessions),
    "acks_received": sum(s["acks_received"] for s in sessions),
    "round_trip_ms": {
        "median": statistics.median(med) if med else None,
        "p95": max(p95) if p95 else None,
    },
}, indent=2))
'
}

echo "=== moreland-bench ==="
echo "binary:     $BIN"
echo "duration:   ${SECONDS_RUN}s x ${RUNS} run(s)"
echo "threshold:  ${THRESHOLD}"
echo "min frames: ${MIN_FRAMES_PER_SESSION} per session"
echo "note:       virtual output must have continuous damage"
echo

RAW=""
for i in $(seq 1 "$RUNS"); do
    printf -- '--- run %d/%d ---\n' "$i" "$RUNS"
    if ! OUT=$("$BIN" --seconds "$SECONDS_RUN" --stats --json 2>/dev/null); then
        echo "error: moreland exited non-zero" >&2
        exit 1
    fi
    [[ -n "$OUT" ]] || { echo "error: no report on stdout; device connected?" >&2; exit 1; }
    RAW+="$OUT"$'\n'
done

if ! CURRENT=$(printf '%s' "$RAW" | merge_runs); then
    exit 2
fi

echo
echo "=== current ==="
echo "$CURRENT"

if [[ "$SAVE" == "1" ]]; then
    # Record what produced this baseline. The frame count is not comparable
    # across different workloads, and a future run against a different
    # animator will fail the comparison for the wrong reason if we do not
    # say what the reference workload was.
    WORKLOAD="${MORELAND_BENCH_WORKLOAD:-unspecified}"
    echo "$CURRENT" | python3 -c '
import json, sys, os
d = json.load(sys.stdin)
d["workload"] = os.environ.get("MORELAND_BENCH_WORKLOAD", "unspecified")
print(json.dumps(d, indent=2))
' > "$BASELINE"
    echo
    echo "wrote baseline to $BASELINE (workload: $WORKLOAD)"
    exit 0
fi

if [[ ! -f "$BASELINE" ]]; then
    echo
    echo "no baseline at $BASELINE — run with --save to create one"
    exit 0
fi

echo
echo "=== vs baseline ==="
python3 - "$BASELINE" "$CURRENT" "$THRESHOLD" <<'PYEOF'
import json, sys
base = json.load(open(sys.argv[1]))
curr = json.loads(sys.argv[2])
thresh = float(sys.argv[3])

def cmp(label, key):
    b = base["round_trip_ms"][key]
    c = curr["round_trip_ms"][key]
    if b is None or c is None:
        print(f"  {label}: baseline={b} current={c}  (skipped)")
        return False
    delta = 100.0 * (c - b) / b
    print(f"  {label}: {b:6.2f} ms -> {c:6.2f} ms  ({delta:+.1f}%)")
    if c > b * thresh:
        print(f"    REGRESSION: {label} is {100.0*(c/b - 1):.1f}% worse than baseline")
        return True
    return False

fail = False
fail |= cmp("median round trip", "median")
fail |= cmp("p95 round trip",    "p95")
sys.exit(1 if fail else 0)
PYEOF

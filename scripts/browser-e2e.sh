#!/usr/bin/env bash
# Run the browser-e2e suite the way it has to be run: one test binary
# at a time, one test at a time, with a fresh geckodriver per binary.
#
# geckodriver holds exactly one WebDriver session. Tests run in
# parallel by default and a test that bails before `client.close()`
# leaks its session, after which every later test "gracefully skips"
# with an `ok` verdict and 0.01s runtime. This script serializes the
# suite, restarts the driver between binaries, and counts the skips so
# a run that did not actually drive a browser cannot pass silently.
#
# Usage: scripts/browser-e2e.sh [test-binary-name ...]
#   RYOKAN_WEBDRIVER_PORT (default 4444), RYOKAN_BROWSER_BIN,
#   RYOKAN_BROWSER_HEADLESS=0 are honored by the harness.
set -u
cd "$(dirname "$0")/.."
PORT="${RYOKAN_WEBDRIVER_PORT:-4444}"
LOG_DIR="${TMPDIR:-/tmp}/ryokan-browser-e2e"
mkdir -p "$LOG_DIR"

if [ "$#" -gt 0 ]; then
    BINARIES=("$@")
else
    mapfile -t BINARIES < <(grep -oE '^name = "htmx_browser_e2e[a-z0-9_]*"' Cargo.toml | cut -d'"' -f2)
fi

driver_pid=""
start_driver() {
    # Anything already on the port (a driver left over from a manual
    # run, holding a stale session) has to go, or the fresh one never
    # binds and `/status` keeps answering from the old one.
    for pid in $(ss -ltnp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u); do
        kill "$pid" 2>/dev/null
    done
    if [ -n "$driver_pid" ]; then wait "$driver_pid" 2>/dev/null; fi
    sleep 0.5
    geckodriver --port="$PORT" > "$LOG_DIR/geckodriver-$1.log" 2>&1 &
    driver_pid=$!
    for _ in $(seq 1 30); do
        if curl -sf "http://127.0.0.1:$PORT/status" 2>/dev/null | grep -q '"ready":true'; then return 0; fi
        sleep 0.2
    done
    echo "geckodriver did not come up ready on port $PORT" >&2
    exit 2
}
trap '[ -n "$driver_pid" ] && kill "$driver_pid" 2>/dev/null' EXIT

passed=0; failed=0; skipped=0; status=0
for bin in "${BINARIES[@]}"; do
    start_driver "$bin"
    log="$LOG_DIR/$bin.log"
    echo "=== $bin"
    if cargo test --features test-support,browser-e2e --test "$bin" -- --test-threads=1 --nocapture > "$log" 2>&1; then :; else status=1; fi
    grep -E '^test .* \.\.\. (ok|FAILED)$|\[skip\]|panicked at|^test result' "$log" | sed -E 's/^test (.*) \.\.\. ok$/  ok   \1/; s/^test (.*) \.\.\. FAILED$/  FAIL \1/; s/^test (.*) \.\.\. \[skip\](.*)$/  SKIP \1/; s/^test result: (.*)$/  ---- \1/' | cut -c1-160
    # Under --nocapture the verdict can land on its own line, so count
    # from the summary line rather than per-test lines.
    p=$(grep -oE 'test result: [a-zA-Z]+\. [0-9]+ passed' "$log" | grep -oE '[0-9]+ passed' | awk '{s+=$1} END {print s+0}')
    f=$(grep -oE '[0-9]+ failed' "$log" | awk '{s+=$1} END {print s+0}')
    s=$(grep -c '\[skip\]' "$log")
    passed=$((passed + p - s)); failed=$((failed + f)); skipped=$((skipped + s))
done
echo "=== browser-e2e: $passed passed, $failed failed, $skipped skipped (logs in $LOG_DIR)"
if [ "$skipped" -gt 0 ]; then
    echo "skips mean no browser session was available for those tests; they are not passes" >&2
    # A run that drove no browser must not exit green. Set
    # RYOKAN_E2E_ALLOW_SKIPS=1 to keep the warning-only behavior.
    if [ "${RYOKAN_E2E_ALLOW_SKIPS:-0}" != "1" ]; then status=1; fi
fi
exit $status

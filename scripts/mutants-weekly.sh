#!/usr/bin/env bash
# Weekly cargo-mutants reporter.
#
# Reads the current `mutants.out/` directory plus a previous-baseline
# JSON snapshot, emits a markdown report on stdout, writes an updated
# baseline JSON to `--output-baseline`, and exits 1 when a regression
# is detected: a mutant that was caught last week is missed this week.
# A drop in the aggregate catch rate is reported but is not by itself a
# regression (adding code adds mutants). Designed to be invoked from
# `.github/workflows/mutants-weekly.yml`.
#
# Exit codes: 0 no regression, 1 regression, 2 bad arguments or missing
# input, 3 empty run (nothing caught or missed; no baseline written).
# The workflow opens an issue on 1 and saves the baseline on 0 or 1.
#
# Usage:
#   scripts/mutants-weekly.sh \
#       --current-dir mutants.out \
#       --baseline mutants.baseline.json \
#       --output-baseline mutants.baseline.next.json \
#       --output-report mutants-report.md
#
# All four args are optional; defaults match the intended workflow
# layout. `--baseline` may point at a non-existent file (first run);
# the script then skips the diff section.
#
# Dependencies: `jq` (parse + emit baseline JSON), `awk` (POSIX; the
# runner's is mawk), `sort`. All standard on ubuntu-latest runners.

set -euo pipefail
# A byte-order sort, so report lists come out the same on every machine.
export LC_ALL=C

# How many flipped mutants each report section lists; the rest are
# counted. Truncated from a variable, never by piping into `head`:
# under `pipefail`, `comm | head -N` fails with SIGPIPE once there are
# more than N lines, which aborted the report before it could exit 1.
MAX_MISSES_LISTED=50
MAX_CATCHES_LISTED=20

CURRENT_DIR="mutants.out"
BASELINE_PATH="mutants.baseline.json"
OUT_BASELINE="mutants.baseline.next.json"
OUT_REPORT="/dev/stdout"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --current-dir) CURRENT_DIR="$2"; shift 2 ;;
        --baseline) BASELINE_PATH="$2"; shift 2 ;;
        --output-baseline) OUT_BASELINE="$2"; shift 2 ;;
        --output-report) OUT_REPORT="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

current_caught_file="$CURRENT_DIR/caught.txt"
current_missed_file="$CURRENT_DIR/missed.txt"

if [[ ! -f "$current_caught_file" || ! -f "$current_missed_file" ]]; then
    echo "missing $current_caught_file or $current_missed_file; did cargo-mutants actually run?" >&2
    exit 2
fi

# wc -l counts trailing-newline-terminated lines; an empty file produces
# 0, a one-line file with no trailing newline produces 0 too. Add a
# `tr -cd '\n' | wc -c` fallback if Ryokan's mutants output ever drops
# the trailing newline. Today's output always has one.
caught=$(wc -l < "$current_caught_file" | tr -d ' ')
missed=$(wc -l < "$current_missed_file" | tr -d ' ')
total=$((caught + missed))
# Merged by the workflow beside caught / missed; reported, not compared.
timeouts=0
unviable=0
[[ -f "$CURRENT_DIR/timeout.txt" ]] && timeouts=$(wc -l < "$CURRENT_DIR/timeout.txt" | tr -d ' ')
[[ -f "$CURRENT_DIR/unviable.txt" ]] && unviable=$(wc -l < "$CURRENT_DIR/unviable.txt" | tr -d ' ')

if [[ "$total" -eq 0 ]]; then
    {
        echo "## Mutation testing: empty run"
        echo
        echo "No caught or missed mutants. Possible cargo-mutants harness failure;"
        echo "check the workflow logs."
    } > "$OUT_REPORT"
    exit 3
fi

# A mutant's identity across runs: its file and description, without
# the `:line:col` position. `src/x.rs:120:9: replace foo -> bool with
# true` and the same mutant at line 125 after an edit above it are one
# mutant; compared with positions, a moved mutant dropped out of the
# diff instead of showing as caught or missed.
mutant_key() {
    sed -E 's/^([^:]+):[0-9]+:[0-9]+: /\1: /'
}

# Keys that moved from one list to the other since the baseline, as
# "<key>\t<count>" lines. Args: the "from" list last run and this run,
# then the "to" list last run and this run (one key per line each).
# Without positions, identical mutants in one function share a key
# (`replace && with || in aggregate` four times), and one copy caught
# while another is missed puts the key in both lists every week, so
# membership in both is not a flip. A key flipped only when its count
# in the "to" list rose and its count in the "from" list fell, and by
# the smaller of the two: a copy added or removed by an edit is not one.
flipped() {
    awk '
        FILENAME == ARGV[1] { from_prev[$0]++; next }
        FILENAME == ARGV[2] { from_cur[$0]++; next }
        FILENAME == ARGV[3] { to_prev[$0]++; next }
        { to_cur[$0]++ }
        END {
            for (k in to_cur) {
                rose = to_cur[k] - to_prev[k]
                fell = from_prev[k] - from_cur[k]
                n = rose < fell ? rose : fell
                if (n > 0) printf "%s\t%d\n", k, n
            }
        }' "$1" "$2" "$3" "$4" | sort
}

# How many mutants a `flipped` list covers (the sum of its counts).
flip_total() {
    awk -F '\t' '{ n += $2 } END { print n + 0 }' <<< "$1"
}

# The first `$2` keys of a `flipped` list `$1`, then "...and N more"
# when there are more.
list_capped() {
    local lines="$1" cap="$2" count
    count=$(printf '%s\n' "$lines" | wc -l | tr -d ' ')
    sed -n "1,${cap}p" <<< "$lines" | while IFS=$'\t' read -r key n; do
        if [[ "$n" -gt 1 ]]; then
            echo "- \`$key\` ($n copies)"
        else
            echo "- \`$key\`"
        fi
    done
    if [[ "$count" -gt "$cap" ]]; then
        echo "- ...and $((count - cap)) more (full lists in the workflow artifact)"
    fi
}

pct=$((caught * 100 / total))

date_iso=$(date -u +%F)

# Emit the new baseline JSON regardless of regression status. The
# next run wants today's snapshot as its "previous" baseline. The
# baseline carries the FULL caught + missed lists so the diff can
# identify per-line regressions, not just aggregate count drift.
jq -n \
    --arg date "$date_iso" \
    --argjson caught "$caught" \
    --argjson missed "$missed" \
    --rawfile missed_list "$current_missed_file" \
    --rawfile caught_list "$current_caught_file" \
    '{
        date: $date,
        caught: $caught,
        missed: $missed,
        missed_list: ($missed_list | split("\n") | map(select(. != ""))),
        caught_list: ($caught_list | split("\n") | map(select(. != "")))
    }' > "$OUT_BASELINE"

# Build the report.
{
    echo "## Weekly mutation testing, $date_iso"
    echo
    echo "**Current:** ${caught} caught / ${missed} missed (${pct}% catch rate, ${total} viable; ${timeouts} timed out, ${unviable} unviable)"

    regression=0

    if [[ -f "$BASELINE_PATH" ]]; then
        prev_caught=$(jq -r '.caught' "$BASELINE_PATH")
        prev_missed=$(jq -r '.missed' "$BASELINE_PATH")
        prev_date=$(jq -r '.date' "$BASELINE_PATH")
        prev_total=$((prev_caught + prev_missed))
        if [[ "$prev_total" -gt 0 ]]; then
            prev_pct=$((prev_caught * 100 / prev_total))
        else
            prev_pct=0
        fi
        caught_delta=$((caught - prev_caught))
        missed_delta=$((missed - prev_missed))
        pct_delta=$((pct - prev_pct))

        echo "**Previous (${prev_date}):** ${prev_caught} caught / ${prev_missed} missed (${prev_pct}% catch rate)"
        printf "**Delta:** %+d caught, %+d missed, %+dpp catch rate\n" \
            "$caught_delta" "$missed_delta" "$pct_delta"
        echo

        # Per-mutant diff: any mutant that was CAUGHT last run and is
        # now MISSED. Aggregate-count drift can be benign (added or
        # removed code shifts mutant counts), but a caught-to-missed
        # flip is a real test-quality regression.
        prev_caught_tmp=$(mktemp)
        prev_missed_tmp=$(mktemp)
        cur_caught_tmp=$(mktemp)
        cur_missed_tmp=$(mktemp)
        # caught/missed.txt carry no timing suffix (unlike the streaming
        # log), and `mutant_key` drops the position, so a mutant matches
        # across runs even when edits above it moved its line. Identical
        # mutants in one function share a key, so `flipped` compares
        # per-key counts rather than membership.
        jq -r '.caught_list[]' "$BASELINE_PATH" | mutant_key > "$prev_caught_tmp"
        jq -r '.missed_list[]' "$BASELINE_PATH" | mutant_key > "$prev_missed_tmp"
        mutant_key < "$current_caught_file" > "$cur_caught_tmp"
        mutant_key < "$current_missed_file" > "$cur_missed_tmp"

        # Newly missed mutants: fewer caught and more missed than last
        # week under one key.
        new_misses=$(flipped "$prev_caught_tmp" "$cur_caught_tmp" "$prev_missed_tmp" "$cur_missed_tmp")
        # Newly caught: fewer missed and more caught. Informational.
        new_catches=$(flipped "$prev_missed_tmp" "$cur_missed_tmp" "$prev_caught_tmp" "$cur_caught_tmp")

        if [[ -n "$new_misses" ]]; then
            echo "### 🚨 Newly missed mutants (regression: $(flip_total "$new_misses") flipped CAUGHT to MISSED)"
            echo
            list_capped "$new_misses" "$MAX_MISSES_LISTED"
            regression=1
            echo
        fi

        if [[ -n "$new_catches" ]]; then
            echo "### ✅ Newly caught mutants (improvement: $(flip_total "$new_catches") flipped MISSED to CAUGHT)"
            echo
            list_capped "$new_catches" "$MAX_CATCHES_LISTED"
            echo
        fi

        rm -f "$prev_caught_tmp" "$prev_missed_tmp" "$cur_caught_tmp" "$cur_missed_tmp"
    else
        echo
        echo "_No previous baseline (first run). Today's snapshot becomes next week's reference._"
        echo
    fi

    # Per-file summary table. Useful even on green runs to surface
    # which files have the lowest catch rates as future-targeted work.
    echo "### Per-file breakdown"
    echo
    echo "| File | Caught | Missed | Catch rate |"
    echo "|------|--------|--------|------------|"
    {
        awk -F: '{print $1}' "$current_caught_file"
        awk -F: '{print $1}' "$current_missed_file"
    } | sort -u | while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        c=$(grep -c "^${f}:" "$current_caught_file" || true)
        m=$(grep -c "^${f}:" "$current_missed_file" || true)
        ftotal=$((c + m))
        if [[ "$ftotal" -eq 0 ]]; then
            continue
        fi
        fpct=$((c * 100 / ftotal))
        printf "| %s | %d | %d | %d%% |\n" "$f" "$c" "$m" "$fpct"
    done

    echo
    echo "_Generated by \`scripts/mutants-weekly.sh\`. Baseline at \`$BASELINE_PATH\`. Next baseline written to \`$OUT_BASELINE\`._"
} > "$OUT_REPORT"

exit "$regression"

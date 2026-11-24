#!/usr/bin/env bash
# Run the browser tests in tests/browser (Playwright Test).
#
# Builds the fixture host the tests talk to, installs the one npm
# dependency on first use, and hands every argument on to Playwright:
#
#   scripts/browser-e2e.sh                       # the whole suite, both engines
#   scripts/browser-e2e.sh --project=chromium    # one engine (or firefox)
#   scripts/browser-e2e.sh settings-deletes      # one file
#   scripts/browser-e2e.sh -g cancel_keeps_row   # tests whose title matches
#   scripts/browser-e2e.sh --headed --workers=1  # watch it run
#
# Needs Node.js and Playwright's browser builds. The first run on a
# machine without them stops with Playwright's own instruction:
#   (cd tests/browser && npx playwright install chromium firefox)
#
# Environment:
#   RYOKAN_CHROMIUM_BIN      another Chromium build to launch
#   RYOKAN_BROWSER_HEADLESS  0 shows the browser window
#   RYOKAN_E2E_WORKERS       how many test files run side by side
#   RUST_LOG                 e.g. ryokan=debug prints the app's log
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --locked --features browser-e2e --bin ryokan-e2e-host

cd tests/browser
if [ ! -d node_modules/@playwright/test ]; then
    # --ignore-scripts: nothing in the lockfile needs an install script.
    npm ci --ignore-scripts --no-audit --no-fund
fi
exec npx playwright test "$@"

// Playwright Test configuration for Ryokan's browser tests.
//
// Run the suite with `scripts/browser-e2e.sh` from the repository root;
// it builds the fixture host first. See `tests/AGENTS.md` ("Browser e2e").
import { defineConfig } from '@playwright/test';

// chromium (default), or firefox / webkit once that Playwright build is
// installed (`npx playwright install firefox`).
const browserName = process.env.RYOKAN_E2E_BROWSER || 'chromium';

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.js',
  outputDir: 'test-results',
  reporter: [['list']],
  // A stray `test.only` would make a green run mean almost nothing.
  forbidOnly: true,
  retries: 0,
  // Tests in one file run in order in one worker against one fixture
  // host; files run side by side, each worker with its own host process.
  fullyParallel: false,
  workers: process.env.RYOKAN_E2E_WORKERS ? Number(process.env.RYOKAN_E2E_WORKERS) : undefined,
  timeout: 30_000,
  expect: { timeout: 5_000 },
  use: {
    browserName,
    headless: process.env.RYOKAN_BROWSER_HEADLESS !== '0',
    // RYOKAN_BROWSER_BIN points at another build of the same engine
    // (a system Chromium, say) in place of Playwright's own.
    launchOptions: process.env.RYOKAN_BROWSER_BIN
      ? { executablePath: process.env.RYOKAN_BROWSER_BIN }
      : {},
    // A desktop-sized window. Tests that care about the breakpoint set
    // their own size.
    viewport: { width: 1366, height: 768 },
    // A failed test keeps its trace (DOM snapshots, network, console).
    // No separate screenshot: taking one at the end of every test waits
    // on the page's fonts and added seconds to tests that end on a
    // navigation.
    trace: 'retain-on-failure',
    screenshot: 'off',
  },
});

// Playwright Test configuration for Ryokan's browser tests.
//
// Run the suite with `scripts/browser-e2e.sh` from the repository root;
// it builds the fixture host first. See `tests/AGENTS.md` ("Browser e2e").
import { defineConfig } from '@playwright/test';

// GitHub Actions sets CI. There a failed test is retried twice, as
// the Rust suite's nextest `ci` profile does, and failures are also
// reported as annotations on the run.
const ci = !!process.env.CI;

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.js',
  outputDir: 'test-results',
  reporter: ci ? [['list'], ['github']] : [['list']],
  // A stray `test.only` would make a green run mean almost nothing.
  forbidOnly: true,
  retries: ci ? 2 : 0,
  // Tests in one file run in order in one worker against one fixture
  // host; files run side by side, each worker with its own host process.
  fullyParallel: false,
  workers: process.env.RYOKAN_E2E_WORKERS ? Number(process.env.RYOKAN_E2E_WORKERS) : undefined,
  timeout: 30_000,
  expect: { timeout: 5_000 },
  // Every test runs on both engines. `--project=chromium` (or firefox)
  // narrows a run to one.
  projects: [
    {
      name: 'chromium',
      use: {
        browserName: 'chromium',
        // RYOKAN_CHROMIUM_BIN launches another Chromium build (a system
        // one, say) in place of Playwright's own.
        launchOptions: process.env.RYOKAN_CHROMIUM_BIN
          ? { executablePath: process.env.RYOKAN_CHROMIUM_BIN }
          : {},
      },
    },
    { name: 'firefox', use: { browserName: 'firefox' } },
  ],
  use: {
    headless: process.env.RYOKAN_BROWSER_HEADLESS !== '0',
    // A desktop-sized window. Tests that care about the breakpoint set
    // their own size.
    viewport: { width: 1366, height: 768 },
    // A failed test keeps its trace (DOM snapshots, network, console).
    // Locally every test records one and a pass throws it away, which
    // costs under a tenth of the run. CI records only when a test is
    // being retried, so a failure there still leaves one.
    // No separate screenshot: taking one at the end of every test waits
    // on the page's fonts and added seconds to tests that end on a
    // navigation.
    trace: ci ? 'on-first-retry' : 'retain-on-failure',
    screenshot: 'off',
  },
});

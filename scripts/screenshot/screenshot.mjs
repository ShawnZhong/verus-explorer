// One-off Playwright script to grab README screenshots (light + dark).
// Kept in its own `scripts/package.json` so playwright (and its ~200MB
// browser download) stays out of the main editor build's deps.
//
// Usage — from the project root, assumes `make serve` is running on
// :8000 in another terminal:
//
//   (cd scripts && npm install && npx playwright install chromium)
//   (cd scripts && node screenshot.mjs)
//
// Writes ../public/screenshot-{light,dark}.png. Waits for the verdict
// section so the screenshots capture a completed verify, not the
// cold-load state.

import { chromium } from 'playwright';
import { mkdirSync } from 'fs';

const SERVE_URL = process.env.URL || 'http://localhost:8000';
// Script runs from scripts/screenshot/; write into project-root public/.
const outDir = new URL('../../public/', import.meta.url).pathname;

mkdirSync(outDir, { recursive: true });

const browser = await chromium.launch();

// Match the body's `max-width: 1200px` + `padding: 1.5rem 1rem` so the
// capture has no horizontal whitespace margin. No deviceScaleFactor —
// keeps the PNG small at display-size resolution. Height chosen to fit
// the default workbench layout (editor + verdict + diag pane) without
// scrolling.
for (const scheme of ['light', 'dark']) {
  const page = await browser.newPage({
    viewport: { width: 1232, height: 820 },
    colorScheme: scheme,
  });
  await page.goto(SERVE_URL, { waitUntil: 'networkidle' });
  // First verify = cold load (wasm compile + Z3 warmup). 60s generous.
  await page.waitForSelector('.verdict', { timeout: 60000 });

  // Kick off a second verify so the timing readout reflects warm-cache
  // latency (what repeat visitors actually experience), not the cold
  // wasm-instantiation one-off. Wait for the button to leave the
  // "Verifying…" state before screenshotting.
  await page.click('#parse-run');
  await page.waitForFunction(
    () => document.getElementById('parse-run-label')?.textContent === 'Verify',
    { timeout: 60000 },
  );

  // Hover a clickable span link so the screenshot demonstrates the
  // output-view's source-jump affordance — the `.cm-span-link:hover`
  // rule flips the text to `--focus-ring` with a solid underline, a
  // visible state worth surfacing.
  const spanLink = page.locator('.cm-span-link').first();
  if (await spanLink.count()) await spanLink.hover();

  // Small settle so CM6 finishes measuring / paints the hover state.
  await page.waitForTimeout(500);
  const out = `${outDir}screenshot-${scheme}.png`;
  await page.screenshot({ path: out, fullPage: false });
  console.log(`wrote ${out}`);
  await page.close();
}

await browser.close();

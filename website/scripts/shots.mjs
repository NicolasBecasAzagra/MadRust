// Screenshot pipeline for the review of the redesign.
//   node scripts/shots.mjs [--url http://localhost:4321/MadRust/] [--tag hero] [--pages /,/roadmap]
// Writes website/.preview/<tag>-<route>-<width>-<scheme>.png
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';
import { resolve } from 'node:path';

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const base = arg('url', 'http://localhost:4321/MadRust');
const tag = arg('tag', 'shot');
const outDir = resolve(process.cwd(), '.preview');
const routes = arg('pages', '/').split(',');
const widths = [375, 768, 1440];
const schemes = ['light', 'dark'];

mkdirSync(outDir, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome' });
const written = [];

for (const route of routes) {
  for (const width of widths) {
    for (const scheme of schemes) {
      const context = await browser.newContext({
        viewport: { width, height: width < 500 ? 812 : 900 },
        deviceScaleFactor: 2,
        colorScheme: scheme,
        reducedMotion: 'reduce',
      });
      const page = await context.newPage();
      const url = route === '/' ? `${base}/` : `${base}${route}`;
      await page.goto(url, { waitUntil: 'networkidle', timeout: 45000 });
      // A full page capture expands the viewport to the height of the document,
      // so any element sized in viewport units (a 100svh hero) would stretch to
      // the length of the page. Pin those elements to the real viewport height
      // before capturing, otherwise the layout of the capture is a lie.
      await page.evaluate((vh) => {
        document.querySelectorAll('*').forEach((el) => {
          const min = parseFloat(getComputedStyle(el).minHeight);
          if (Number.isFinite(min) && min > vh * 1.2) {
            el.style.minHeight = `${vh}px`;
          }
        });
      }, 900);
      await page.waitForTimeout(700);
      const name = route === '/' ? 'home' : route.replace(/^\//, '').replace(/\//g, '-');
      const file = resolve(outDir, `${tag}-${name}-${width}-${scheme}.png`);
      await page.screenshot({ path: file, fullPage: true });
      written.push(file.replace(`${process.cwd()}/`, ''));
      await context.close();
    }
  }
}

await browser.close();
console.log(written.join('\n'));

// Renders the Open Graph image of the site.
//
// The image is a real page rendered by the browser, so it uses the fonts and
// the tokens of the site instead of an approximation of them.
//
//   node scripts/og.mjs                      # uses scripts/og.html
//   node scripts/og.mjs --url http://.../    # renders the template in place
import { chromium } from 'playwright';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, '..');
const templatePath = join(root, 'scripts/og.html');
const outPath = join(root, 'public/og.png');

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const base = arg('url', 'http://localhost:4321/MadRust');
const width = Number(arg('width', 1200));
const height = Number(arg('height', 630));

if (!existsSync(templatePath)) {
  console.error(`missing template: ${templatePath}`);
  process.exit(1);
}

const html = readFileSync(templatePath, 'utf8');
let css = readFileSync(join(root, 'src/styles/global.css'), 'utf8');

// The stylesheet points at the fonts with the base path of the site, which
// does not exist in a page rendered from memory. Inline them so the image
// carries the same typefaces as the site.
css = css.replace(/url\(["']?\/MadRust\/fonts\/([^"')]+)["']?\)/g, (_match, file) => {
  const font = readFileSync(join(root, 'public/fonts', file));
  return `url("data:font/woff2;base64,${font.toString('base64')}")`;
});

const browser = await chromium.launch({ channel: 'chrome' });
const page = await browser.newPage({ viewport: { width, height }, deviceScaleFactor: 1 });
await page.setContent(html.replace('/*__CSS__*/', css), { waitUntil: 'load' });
await page.evaluate(() => document.fonts.ready);
await page.waitForTimeout(400);
const buffer = await page.screenshot({ type: 'png' });
await browser.close();

writeFileSync(outPath, buffer);
console.log(`Generated ${outPath} (${width}x${height}, ${(buffer.length / 1024).toFixed(1)} kB)`);

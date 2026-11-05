// Builds the animated banner of the README from the live site.
//
//   node scripts/make-banner.mjs --url http://localhost:4321/MadRust/ --out ../docs/boot.gif
//
// The banner is the boot sequence of the real kernel, with the mark of the
// project. It is assembled in the browser, where the styles of the site are
// already loaded, and the animation is driven frame by frame so the result is
// deterministic.
import { chromium } from 'playwright';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readdirSync, rmSync } from 'node:fs';
import { resolve, join } from 'node:path';

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const url = arg('url', 'http://localhost:4321/MadRust/');
const out = resolve(process.cwd(), arg('out', '../docs/boot.gif'));
const width = Number(arg('width', 1280));
const height = Number(arg('height', 480));
const fps = Number(arg('fps', 15));
const duration = Number(arg('duration', 9));
const framesDir = resolve(process.cwd(), '.preview/banner-frames');

rmSync(framesDir, { recursive: true, force: true });
mkdirSync(framesDir, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome' });
const context = await browser.newContext({
  viewport: { width: 1440, height: 900 },
  deviceScaleFactor: 1,
  colorScheme: 'light',
  reducedMotion: 'no-preference',
});
const page = await context.newPage();
await page.goto(url, { waitUntil: 'networkidle', timeout: 45000 });
await page.waitForSelector('.hero-terminal .terminal-line', { timeout: 15000 });

// Build the banner inside the page: the mark, the wordmark, and the very same
// terminal element the site shows, so the animation is the site's animation.
await page.evaluate(
  async ({ width, height }) => {
    const logo = await fetch('/MadRust/logo.svg').then((r) => r.text());
    const terminal = document.querySelector('.hero-terminal');
    const banner = document.createElement('div');
    banner.id = 'readme-banner';
    banner.style.cssText = `position:fixed;left:0;top:0;z-index:9999;width:${width}px;height:${height}px;display:flex;flex-direction:column;background:var(--paper);border:1px solid var(--border-strong);font-family:var(--font-body);overflow:hidden`;

    const header = document.createElement('div');
    header.style.cssText =
      'display:flex;align-items:center;gap:12px;padding:18px 24px;border-bottom:1px solid var(--rule)';
    const mark = document.createElement('span');
    mark.style.cssText = 'display:flex;width:26px;height:26px;color:var(--ink)';
    mark.innerHTML = logo;
    const word = document.createElement('span');
    word.style.cssText =
      'font-family:var(--font-display);font-weight:800;font-variation-settings:"wdth" 75,"wght" 800;font-size:22px;letter-spacing:-0.02em;color:var(--ink)';
    word.textContent = 'MadRust';
    const claim = document.createElement('span');
    claim.style.cssText =
      'font-size:13px;color:var(--ink-secondary);margin-left:8px;letter-spacing:0.02em';
    claim.textContent = 'An operating system for local AI agents, written in Rust.';
    const meta = document.createElement('span');
    meta.style.cssText =
      'margin-left:auto;font-size:12px;color:var(--ink-tertiary);text-transform:uppercase;letter-spacing:0.08em';
    meta.textContent = 'x86_64 · Linux ABI · GPL-2.0-only';
    header.append(mark, word, claim, meta);

    const body = document.createElement('div');
    body.style.cssText = 'flex:1;padding:18px 24px;display:flex;flex-direction:column;justify-content:center';
    const clone = terminal.cloneNode(true);
    clone.style.width = '100%';
    clone.style.height = 'auto';
    clone.style.minHeight = '0';
    body.append(clone);

    banner.append(header, body);
    document.body.append(banner);
    document.body.style.overflow = 'hidden';
  },
  { width, height }
);

await page.waitForTimeout(400);

const timeline = await page.evaluate(() => {
  const animations = document.getAnimations();
  const end = animations.reduce((max, a) => {
    const t = a.effect?.getComputedTiming?.().endTime ?? 0;
    return Math.max(max, Number.isFinite(t) ? t : 0);
  }, 0);
  animations.forEach((a) => a.pause());
  return { end, count: animations.length };
});

const total = Math.min(duration, timeline.end || duration);
const totalFrames = Math.max(2, Math.round(total * fps));
const banner = await page.$('#readme-banner');

for (let frame = 0; frame < totalFrames; frame += 1) {
  const time = (frame / fps) * 1000;
  await page.evaluate((t) => {
    document.getAnimations().forEach((a) => {
      try {
        a.currentTime = t;
      } catch {
        /* an animation that cannot be seeked is left where it is */
      }
    });
  }, time);
  await page.waitForTimeout(16);
  await banner.screenshot({ path: join(framesDir, `frame-${String(frame).padStart(4, '0')}.png`) });
}

await browser.close();

const frames = readdirSync(framesDir).filter((f) => f.endsWith('.png')).sort();
if (frames.length === 0) {
  console.error('no frames captured');
  process.exit(1);
}

mkdirSync(resolve(out, '..'), { recursive: true });
execFileSync('ffmpeg', [
  '-y',
  '-framerate', String(fps),
  '-i', join(framesDir, 'frame-%04d.png'),
  '-vf',
  `scale=${width}:${height}:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer:bayer_scale=4`,
  '-loop', '0',
  out,
], { stdio: 'inherit' });

console.log(
  JSON.stringify(
    {
      out,
      frames: frames.length,
      seconds: Number((frames.length / fps).toFixed(2)),
      animations: timeline.count,
      timelineEnd: Number((timeline.end ?? 0).toFixed(2)),
    },
    null,
    2
  )
);

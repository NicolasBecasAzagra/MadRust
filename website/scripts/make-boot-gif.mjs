// Builds the animated boot log of the README banner from the live site.
//
//   node scripts/make-boot-gif.mjs --url http://localhost:4321/MadRust/ \
//        --selector "[data-boot-log]" --out ../docs/boot.gif \
//        --width 1280 --height 480 --fps 24 --duration 9
//
// The animation of the site is paused and driven frame by frame, so the GIF is
// deterministic and does not depend on how fast the machine renders.
import { chromium } from 'playwright';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readdirSync, rmSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

const url = arg('url', 'http://localhost:4321/MadRust/');
const selector = arg('selector', '[data-boot-log]');
const out = resolve(process.cwd(), arg('out', '../docs/boot.gif'));
const width = Number(arg('width', 1280));
const height = Number(arg('height', 480));
const fps = Number(arg('fps', 24));
const duration = Number(arg('duration', 9));
const framesDir = resolve(process.cwd(), '.preview/frames');

rmSync(framesDir, { recursive: true, force: true });
mkdirSync(framesDir, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome' });
const context = await browser.newContext({
  viewport: { width, height },
  deviceScaleFactor: 2,
  colorScheme: 'light',
  reducedMotion: 'no-preference',
});
const page = await context.newPage();
await page.goto(url, { waitUntil: 'networkidle', timeout: 45000 });
await page.waitForSelector(selector, { timeout: 15000 });

// Freeze every animation and find out how long the longest one runs.
const timeline = await page.evaluate((sel) => {
  const animations = document.getAnimations();
  const end = animations.reduce((max, a) => {
    const t = a.effect?.getComputedTiming?.().endTime ?? 0;
    return Math.max(max, Number.isFinite(t) ? t : 0);
  }, 0);
  animations.forEach((a) => a.pause());
  return { end, count: animations.length };
}, selector);

const total = Math.min(duration, timeline.end || duration);
const totalFrames = Math.max(2, Math.round(total * fps));

for (let frame = 0; frame < totalFrames; frame += 1) {
  const time = (frame / fps) * 1000;
  await page.evaluate(
    ({ t }) => {
      document.getAnimations().forEach((a) => {
        try {
          a.currentTime = t;
        } catch {
          /* an animation that cannot be seeked is left where it is */
        }
      });
    },
    { t: time }
  );
  await page.waitForTimeout(16);
  const element = await page.$(selector);
  await element.screenshot({ path: join(framesDir, `frame-${String(frame).padStart(4, '0')}.png`) });
}

await browser.close();

const frames = readdirSync(framesDir).filter((f) => f.endsWith('.png')).sort();
if (frames.length === 0) {
  console.error(`no frames captured for selector ${selector}`);
  process.exit(1);
}

mkdirSync(resolve(out, '..'), { recursive: true });
execFileSync('ffmpeg', [
  '-y',
  '-framerate', String(fps),
  '-i', join(framesDir, 'frame-%04d.png'),
  '-vf',
  `scale=${width}:-1:flags=lanczos,pad=${width}:${height}:(ow-iw)/2:(oh-ih)/2:color=0x00000000,split[a][b];[a]palettegen=max_colors=192[p];[b][p]paletteuse=dither=bayer:bayer_scale=4`,
  '-loop', '0',
  out,
], { stdio: 'inherit' });

console.log(
  JSON.stringify(
    {
      out,
      frames: frames.length,
      fps,
      duration: Number((frames.length / fps).toFixed(2)),
      animations: timeline.count,
      timelineEnd: Number(timeline.end?.toFixed?.(2) ?? timeline.end),
      exists: existsSync(out),
    },
    null,
    2
  )
);

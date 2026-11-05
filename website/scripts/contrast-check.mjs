import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(resolve(__dirname, '../src/styles/global.css'), 'utf8');

function parseHex(hex) {
  const clean = hex.replace('#', '');
  const r = parseInt(clean.slice(0, 2), 16) / 255;
  const g = parseInt(clean.slice(2, 4), 16) / 255;
  const b = parseInt(clean.slice(4, 6), 16) / 255;
  return { r, g, b };
}

function luminance({ r, g, b }) {
  const [rs, gs, bs] = [r, g, b].map((c) => {
    return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * rs + 0.7152 * gs + 0.0722 * bs;
}

function ratio(a, b) {
  const l1 = luminance(parseHex(a)) + 0.05;
  const l2 = luminance(parseHex(b)) + 0.05;
  return l1 > l2 ? l1 / l2 : l2 / l1;
}

function extractVars(block) {
  const vars = {};
  for (const line of block.split('\n')) {
    const m = line.match(/--([\w-]+):\s*(#[0-9a-fA-F]{6});/);
    if (m) vars[m[1]] = m[2];
  }
  return vars;
}

const lightBlock = css.match(/:root\s*\{[\s\S]*?\n\}/)[0];
const darkBlock = css.match(/@media\s*\(prefers-color-scheme:\s*dark\)\s*\{[\s\S]*?\n\s*\}\s*\n\}/)[0];

const light = extractVars(lightBlock);
const dark = extractVars(darkBlock);

const combos = [
  ['ink', 'paper'],
  ['ink', 'paper-elevated'],
  ['ink', 'paper-sunk'],
  ['ink-secondary', 'paper'],
  ['ink-secondary', 'paper-elevated'],
  ['ink-secondary', 'paper-sunk'],
  ['ink-tertiary', 'paper'],
  ['ink-tertiary', 'paper-elevated'],
  ['ink-tertiary', 'paper-sunk'],
  ['accent', 'paper'],
  ['accent', 'paper-elevated'],
  ['accent', 'paper-sunk'],
  ['accent-hover', 'paper'],
  ['accent-hover', 'paper-elevated'],
  ['accent-hover', 'paper-sunk'],
  ['ink-inverse', 'accent'],
  ['ink-inverse', 'accent-hover'],
];

let fail = false;

for (const theme of [
  { name: 'light', vars: light },
  { name: 'dark', vars: dark },
]) {
  console.log(`\n${theme.name.toUpperCase()} THEME`);
  for (const [fg, bg] of combos) {
    const r = ratio(theme.vars[fg], theme.vars[bg]);
    const label = r >= 4.5 ? 'PASS' : r >= 3 ? 'LARGE' : 'FAIL';
    if (label === 'FAIL') fail = true;
    console.log(`  ${fg} on ${bg}: ${r.toFixed(2)}:1 [${label}]`);
  }
}

if (fail) {
  console.log('\nSome combinations fall below 4.5:1.');
  process.exit(1);
} else {
  console.log('\nAll combinations meet AA for normal text (4.5:1).');
}

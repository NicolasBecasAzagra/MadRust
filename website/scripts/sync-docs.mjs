#!/usr/bin/env node
// Copies the documentation of the repository into the website as pages.
//
// The source of truth is the markdown of the repository: docs/*.md and
// REPORT.md. This script only adds the frontmatter that Astro needs, so a
// document is never written twice. The generated files are ignored by git and
// rebuilt by `npm run build` (the prebuild hook) and `npm run dev`.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const websiteRoot = resolve(here, '..');
const repoRoot = resolve(websiteRoot, '..');

/** slug, source file relative to the repository root, order in the index */
const sources = [
  { slug: 'architecture', file: 'docs/ARCHITECTURE.md', order: 1 },
  { slug: 'boot', file: 'docs/BOOT.md', order: 2 },
  { slug: 'benchmarking', file: 'docs/BENCHMARKING.md', order: 3 },
  { slug: 'quantization', file: 'docs/QUANTIZATION.md', order: 4 },
  { slug: 'learning', file: 'docs/LEARNING.md', order: 5 },
  { slug: 'vm-setup', file: 'docs/VM-SETUP.md', order: 6 },
  { slug: 'report', file: 'REPORT.md', order: 7 },
];

const LAYOUT = '../../layouts/DocLayout.astro';

function firstHeading(markdown) {
  const match = markdown.match(/^#\s+(.+)$/m);
  return match ? match[1].trim() : 'Untitled';
}

function firstParagraph(markdown) {
  const withoutCode = markdown.replace(/```[\s\S]*?```/g, '');
  const blocks = withoutCode
    .split(/\n{2,}/)
    .map((block) => block.trim())
    .filter((block) => block.length > 0);
  for (const block of blocks) {
    if (block.startsWith('#')) continue;
    if (block.startsWith('|') || block.startsWith('-') || block.startsWith('*')) continue;
    return block.replace(/\s+/g, ' ').slice(0, 200);
  }
  return '';
}

function escape(value) {
  return value.replace(/"/g, '\\"');
}

/**
 * The layout prints the title of the document, so the markdown keeps its own
 * first heading out of the body: otherwise the page would carry two h1 and
 * the title would be printed twice.
 */
function stripLeadingHeading(markdown) {
  const match = markdown.match(/^(#\s+.+?)(\r?\n)+/);
  if (!match) return markdown;
  return markdown.slice(match[0].length);
}

const outputDir = join(websiteRoot, 'src', 'pages', 'docs');
mkdirSync(outputDir, { recursive: true });

for (const source of sources) {
  const raw = readFileSync(join(repoRoot, source.file), 'utf8');
  const title = firstHeading(raw);
  const description = firstParagraph(raw);
  const markdown = stripLeadingHeading(raw);
  const frontmatter = [
    '---',
    `title: "${escape(title)}"`,
    `description: "${escape(description)}"`,
    `source: "${source.file}"`,
    `order: ${source.order}`,
    `layout: ${LAYOUT}`,
    '---',
    '',
  ].join('\n');
  writeFileSync(join(outputDir, `${source.slug}.md`), `${frontmatter}${markdown}`, 'utf8');
  console.log(`docs: ${source.file} -> src/pages/docs/${source.slug}.md`);
}

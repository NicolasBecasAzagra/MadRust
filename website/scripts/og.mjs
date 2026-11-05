import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import sharp from 'sharp';

const __dirname = dirname(fileURLToPath(import.meta.url));
const outPath = join(__dirname, '../public/og.png');

const width = 1200;
const height = 630;

const svg = `
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">
  <rect width="${width}" height="${height}" fill="#0a0a0a"/>
  <defs>
    <pattern id="grid" width="48" height="48" patternUnits="userSpaceOnUse">
      <path d="M 48 0 L 0 0 0 48" fill="none" stroke="#1f1f1f" stroke-width="1"/>
    </pattern>
  </defs>
  <rect width="${width}" height="${height}" fill="url(#grid)"/>
  <rect x="80" y="235" width="1040" height="4" rx="2" fill="#b7410e"/>
  <text x="80" y="190" font-family="-apple-system, BlinkMacSystemFont, 'SF Pro Display', 'Inter', sans-serif" font-size="72" font-weight="600" fill="#f5f5f5" letter-spacing="-0.03em">MadRust</text>
  <text x="80" y="310" font-family="-apple-system, BlinkMacSystemFont, 'SF Pro Display', 'Inter', sans-serif" font-size="34" font-weight="400" fill="#a8a8a8">An operating system for local AI agents,</text>
  <text x="80" y="355" font-family="-apple-system, BlinkMacSystemFont, 'SF Pro Display', 'Inter', sans-serif" font-size="34" font-weight="400" fill="#a8a8a8">written in Rust from the kernel up.</text>
  <text x="80" y="540" font-family="ui-monospace, SFMono-Regular, 'SF Mono', Menlo, monospace" font-size="18" fill="#6e6e6e">github.com/NicolasBecasAzagra/MadRust</text>
</svg>
`;

const buffer = await sharp(Buffer.from(svg))
  .png({ compressionLevel: 9 })
  .toBuffer();

writeFileSync(outPath, buffer);
console.log(`Generated ${outPath} (${width}x${height})`);

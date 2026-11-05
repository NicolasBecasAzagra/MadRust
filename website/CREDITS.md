# Credits

Every external resource used by this site, with its license. The project is
`GPL-2.0-only`; nothing below is paid, and nothing below is incompatible with
that license. No stock photography, no icon sets from third parties, no
templates: every visual element of the site is drawn in this repository.

## Typefaces

Self-hosted through Fontsource, no CDN and no external request at runtime.

| Family | Package | Role | License |
| --- | --- | --- | --- |
| Martian Mono | `@fontsource-variable/martian-mono` | Display: titles, figures, the wordmark | SIL Open Font License 1.1 |
| JetBrains Mono | `@fontsource-variable/jetbrains-mono` | Body, data, code, navigation | SIL Open Font License 1.1 |

The files are copied to `public/fonts/` and served from the site itself, with
the two latin subsets preloaded.

## Code and tooling (development only, not shipped to the reader)

| Tool | Use | License |
| --- | --- | --- |
| Astro | Static site generator | MIT |
| @astrojs/mdx, @astrojs/sitemap | Integrations | MIT |
| Playwright | Screenshots at 375/768/1440, light and dark | Apache-2.0 |
| sharp | Open Graph image generation | Apache-2.0 |
| ffmpeg | Encoding the boot animation as a GIF for the README | LGPL-2.1 / GPL-2.0 |

The site ships no JavaScript framework and no client-side bundle. The pages
are HTML, CSS and a small amount of vanilla script.

## Agent skills used during the redesign

Installed under `.opencode/skill/` (not versioned) and read as guidance.

| Skill | Source | License |
| --- | --- | --- |
| frontend-design | `github.com/anthropics/skills` | Complete terms in its bundled `LICENSE.txt` |
| theme-factory | `github.com/anthropics/skills` | Complete terms in its bundled `LICENSE.txt` |
| webapp-testing | `github.com/anthropics/skills` | Complete terms in its bundled `LICENSE.txt` |
| better-typography, better-interface, better-accessibility, better-layout, better-colors, better-writing | `github.com/jakubkrehel/skills` | MIT |
| design-system | `github.com/nextlevelbuilder/ui-ux-pro-max-skill` | MIT |
| web-design-guidelines | `github.com/vercel-labs/web-interface-guidelines` | MIT |

## References studied

These were read as references for principles only. No markup, no code, no
assets and no text were copied from them.

| Site | What was taken |
| --- | --- |
| oxide.computer | Mono reserved for labels and figures, measured type scale, numbered figures |
| zed.dev | The product in the hero instead of an illustration of it |
| redox-os.org | Release-note structure for a kernel project |
| ghostty.org | Mono-first interface, one claim, verifiable facts |
| linear.app | Density, 1 px separators, motion as a response to an action |
| fly.io | Three typographic roles, one accent used for action and state |
| rust-lang.org | One display face, one text face, semantic color |
| tailwindcss.com/docs/theme | Tokens as an API, spacing base, paired line heights |
| d3js.org, observablehq.com/plot, datawrapper.de, graphpad.com | Log scale constraints, why bars are wrong on a log axis |

See `MOODBOARD.md` for what each reference contributed.

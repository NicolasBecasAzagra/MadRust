# MadRust website

## What this is

The public site of MadRust: a small static website built with [Astro](https://astro.build)
7 in static-output mode. There is no framework runtime and no client-side
JavaScript bundle, so every page is plain HTML, CSS and a little inline script.

Hosted on GitHub Pages at <https://nicolasbecasazagra.github.io/MadRust/>.

## Local development

```sh
cd website
npm install
npm run dev
```

The dev server runs on <http://localhost:4321/MadRust/>. The base path is
`/MadRust` (same as production), so the home page is not at `/`: use
`http://localhost:4321/MadRust/` or any route below it.

Build and preview the production output:

```sh
npm run build     # writes website/dist/
npm run preview   # serves website/dist/ at http://localhost:4321/MadRust/
```

`npm run check` runs `astro check` for type and content errors.

## Where the content lives

- **`src/data/project.ts` is the single source of truth for the copy.** Every
  editable dataset lives there: site meta, layers, milestones, benchmarks, FAQ,
  build instructions, docs listing and risks. To change a number shown on the
  site, change it in this file.
- **The documents come from the repository root.** `docs/*.md` and `REPORT.md`
  are copied into `src/pages/docs/*.md` by `scripts/sync-docs.mjs`, which runs
  automatically as the `predev` and `prebuild` hook. Never edit the generated
  copies under `src/pages/docs/`: they are git-ignored and overwritten on the
  next build. Edit the markdown at the root of the repository.
- **Internal links go through `src/lib/paths.ts`.** The site is served from the
  `/MadRust` subpath, configured as `base` in `astro.config.mjs`. Always build
  hrefs with the `url()` helper from that module instead of hard-coding paths,
  otherwise links break in production.

## Route map

| Route             | Page                                      |
| ----------------- | ----------------------------------------- |
| `/`               | Home                                      |
| `/roadmap`        | Roadmap / milestones                      |
| `/benchmarks`     | Benchmark results                         |
| `/download`       | Build and run instructions                |
| `/docs`           | Documentation index                       |
| `/docs/<slug>`    | One page per root markdown document       |
| `/changelog`      | Changelog generated from `git log`        |
| `/faq`            | Frequently asked questions                |
| `404`             | Not found                                 |

Because the changelog is generated from `git log` at build time, the CI checkout
uses `fetch-depth: 0` (full history).

## Deploy

Push to `main`. The GitHub Actions workflow `.github/workflows/pages.yml` (job
`Pages`) installs dependencies with `npm ci`, builds the site and publishes
`website/dist` to GitHub Pages. No secrets are configured: the deploy
authenticates through the workflow's OIDC token.

Result: <https://nicolasbecasazagra.github.io/MadRust/>.

### One manual step, once

In the repository, go to **Settings → Pages → Build and deployment** and set
**Source = "GitHub Actions"**. This is the only step a workflow cannot do for
you, and the deploy fails until it is selected.

## Buying a domain later

Do these in order:

1. Add `website/public/CNAME` containing the domain (for example
   `madrust.dev`).
2. In `website/astro.config.mjs`, set `base: '/'` and
   `site: 'https://your-domain.com'`.
3. In GitHub, **Settings → Pages → Custom domain** = `your-domain.com`, then
   enable **Enforce HTTPS**.
4. Configure DNS at your provider: `CNAME www → nicolasbecasazagra.github.io`,
   and an `A`/`ALIAS` record for the apex domain. Then approve the domain when
   GitHub asks (the **Approve** dialog under Settings → Pages).

With `base: '/'` the site still works under the repository path only while
GitHub Pages redirects to the custom domain; the simple, supported setup is to
serve it from the domain.

## Contributing notes

- New section on the site: add its data to `src/data/project.ts` and render it
  from a page in `src/pages/`.
- New document in the repository: add the `docs/*.md` entry to the `docs` list
  in `src/data/project.ts` **and** to the `sources` array in
  `scripts/sync-docs.mjs`, so it is copied and listed.

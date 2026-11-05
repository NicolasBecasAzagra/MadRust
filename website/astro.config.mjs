// @ts-check
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';

// The site is static. GitHub Pages serves it from https://<user>.github.io/MadRust,
// so `base` must carry the repository name. When a domain is bought, set
// `base: '/'` here and add a CNAME file with the domain.
export default defineConfig({
  site: 'https://nicolasbecasazagra.github.io',
  base: '/MadRust',
  trailingSlash: 'ignore',
  build: {
    format: 'directory',
  },
  integrations: [mdx(), sitemap()],
});

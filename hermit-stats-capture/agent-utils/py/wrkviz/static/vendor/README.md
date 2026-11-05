# Vendored browser assets

## markdown-it 15.0.0

- Upstream: https://github.com/markdown-it/markdown-it
- npm package: https://www.npmjs.com/package/markdown-it/v/15.0.0
- Tarball: https://registry.npmjs.org/markdown-it/-/markdown-it-15.0.0.tgz
- npm SHA-1: `dc199771f75b01d792316e5b524855b3973868e2`
- License: MIT; see `markdown-it-LICENSE.txt`
- Vendored file: `package/dist/browser/markdown-it.umd.min.js`, renamed to
  `markdown-it-15.0.0.min.js`

The generated timeline is intentionally self-contained, so this pinned browser bundle is copied
into each built archive instead of being fetched from a CDN. The application initializes it with
raw HTML disabled before rendering transcript-derived Markdown.

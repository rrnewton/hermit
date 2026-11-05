# agent-utils website

The public website, published by GitHub Pages at <https://rrnewton.github.io/agent-utils/>. It
is the overview layer: the suite's purpose, tools and architecture, with links down into the
repository's own documents, which stay the authority.

| File | What it is |
|---|---|
| `index.html` | Overview: why the suite exists, the architecture figure, the tools, adoption, engineering. |
| `architecture.html` | Layers, a delegated task end to end, the harness directory, where state lives. |
| `tools.html` | One section per tool, anchored by tool name (`tools.html#wrkslots`). |
| `vibe-talk.html` | vibe-talk: what it is for, five demo clips, what in them is real, providers and maturity. |
| `media/vibe-talk/` | The demo clips: one GIF and one still poster PNG per clip. |
| `style.css` | The only stylesheet, with light and dark palettes. |
| `favicon.svg` | The site icon. |
| `.nojekyll` | Tells a branch-based Pages build not to run Jekyll. The Actions deployment ignores it. |
| `demos/` | **Not published.** The tools that record the demo clips; see `demos/vibe-talk/README.md`. |
| `README.md` | **Not published.** These maintainer notes, which GitHub shows when you browse `site/`. |

## Rules

- Hand-written HTML and one stylesheet. No build step, no frameworks, and no external resources
  of any kind: no web fonts, CDNs, analytics or trackers. The pages must read well with
  JavaScript off; the only script stores the System/Light/Dark theme choice.
- Every page carries the same Content-Security-Policy `<meta>`, which allows this origin and the
  hash of the inline theme script, and nothing else. If you change that script,
  `scripts/check_site.py` prints the hash to put in every page's policy. Inline `style`
  attributes, `<style>` elements and event-handler attributes are refused for the same reason.
- Pages share one header, skip link and footer. A new page copies them from an existing page and
  is added to every page's navigation.
- Link pages to each other relatively. Link repository content as
  `https://github.com/rrnewton/agent-utils/blob/main/<path>` for files and `/tree/main/<dir>`
  for directories, and only to tracked paths that are not symlinks.
- Introduce a tool by the user's problem, the useful outcome, then its dependencies, and state its
  maturity the way its own documents do. This is a public page: never name a project that
  consumes agent-utils, and keep host- or organization-specific details out.
- Every image has `alt`. GIFs stay at or under 3 MB.
- Nothing moves until the visitor asks. A demo clip shows its still poster (`<img
  class="clip-poster">`) after a `<details class="clip-play">` whose `<summary>` says "Play the
  clip" and whose body is the GIF; opening it hides the poster and closing it stops the animation,
  which is what WCAG 2.2.2 requires of anything that moves for more than five seconds. Every
  `<img>` has its own `width` and `height` so the page does not shift when it loads, and
  `loading="lazy"`; a plain link beside the clip opens the animation. Say what the clip was
  recorded against. The vibe-talk clips ran against local mocks, and the page says so.
- `demos/` and this `README.md` are repository source, never website. The checker does not treat
  them as website files, refuses a page link to them, and leaves them out of what it stages for
  Pages. Link them by their GitHub URL.
- Every published file must be tracked: `git add` a new page, GIF or poster before you run the
  checker. CI and the Pages deploy check out a clean tree, so an untracked file that previews
  fine locally would be missing there, and the checker refuses it.

## Check and preview

```sh
python3 scripts/check_site.py --self-test
python3 scripts/check_site.py
python3 -m http.server --directory site 8000    # then open http://127.0.0.1:8000/
```

Preview over HTTP rather than `file://`: the page policy allows only `'self'`, which some
browsers do not grant to local files. `make validate` runs the checker on every change, because
renaming a document or rewording a heading anywhere in the repository can break a link here.

## Deploy

`.github/workflows/pages.yml` runs on every push to `main` that touches `site/`,
`scripts/check_site.py` or the workflow, and on manual dispatch. `python3 scripts/check_site.py
--stage DIR` checks the pages and then copies exactly the published files, everything here except
`demos/` and this `README.md`, into a staging directory, and the workflow publishes that.

It needs one repository setting, which no workflow can change: Settings, Pages, Build and
deployment, Source: GitHub Actions. Until it is set, the workflow's configure-pages step fails
with "Get Pages site failed" (Not Found). After setting it, run the workflow once by hand
(Actions, pages, Run workflow; or `gh workflow run pages.yml`), then open
<https://rrnewton.github.io/agent-utils/> to confirm it serves the overview. Links to the site
from outside `site/`, such as the README's, should land only after that first deploy succeeds.

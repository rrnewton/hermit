# Adversarial review evidence

These reports record independent post-port reviews of the paired command-line
tools and their distribution artifacts. They are evidence, not a substitute for
the executable contract: every reported behavior is also pinned by unit or
differential tests.

Reviews completed from 2026-08-05 through 2026-08-09:

- [tick-hub](tick-hub.md)
- [packaging and documentation](packaging-and-docs.md)
- [dagrun and cpuset-alloc](dagrun.md)
- [pr-landing-planner](pr-landing-planner.md)
- [herdr-run](herdr-run.md)
- [herdr-agent](herdr-agent.md)
- [repository-local Rust source launchers](rust-source-launchers.md)

Later, and a different kind of review — a LOOKED-AT one rather than a read-the-code one, because
the subject is a layout and no test in the repository has an opinion about a layout:

- [vibe-talk `/voice` — the desktop composition](vibe-talk-desktop-ui.md) (2026-08-19)

## Where a tool's own related-work document lives

A related-work document belongs to one tool, so it lives **in that tool's own folder, beside that
tool's README, named `RELATED_WORK.md`** — not here. This directory is for review evidence, which
is not tool-scoped in the same way: a review is a dated record of an audit, and several of the
reviews below cover more than one command.

| Tool | Its related work |
| --- | --- |
| `herdr-run` | [`common/docs/herdr-run/RELATED_WORK.md`](../common/docs/herdr-run/RELATED_WORK.md) |
| `wrkviz` | [`common/docs/wrkviz/RELATED_WORK.md`](../common/docs/wrkviz/RELATED_WORK.md) |
| `vibe-talk` | [`vibe-talk/RELATED_WORK.md`](../vibe-talk/RELATED_WORK.md) |

For a paired tool the folder is the one under `common/docs/`, which holds that tool's authoritative
README source. The published `py/` and `rs/` trees are deliberately not the home: they are the
distributable package trees, and `scripts/embed_userguides.py --check` requires their contents to
carry no development-history or sibling-language references. `vibe-talk` is a service with a single
implementation and no `common/docs/` entry, so its document sits next to its own README.

Supporting research too long to live in a tool's document is kept here:

- [wrkviz comparative analysis](research/wrkviz-comparative-analysis.md)

The standard reproducibility entry points are:

```sh
python3 scripts/embed_userguides.py --check
make check-python-packages
make check-rust-packages
make cross
```

Randomized differentials always record their fixture count and seed so a failed
case can be regenerated exactly.

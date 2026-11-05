# Shared documentation

This directory holds settled user-facing material and public related-work comparisons.
Working designs and research belong in dated `ai_docs/transient/` records.

Operator documentation is primarily available through each command’s `quickstart`,
`userguide`, and subcommand `--help`. Some tools store their authoritative CLI prose
here and inject distribution-specific installation text:

| Tool | Shared sources | Generated package documents |
| --- | --- | --- |
| `dagrun` | `README.template.md`, `USER_GUIDE.template.md` | `rendered/python/`, `rendered/rust/` |
| `tick-hub` | `README.template.md`, `USER_GUIDE.template.md` | `rendered/python/`, `rendered/rust/` |
| `pr-landing-planner` | `README.template.md`, `USER_GUIDE.template.md` | `rendered/python/`, `rendered/rust/` |
| `herdr-run` | `README.template.md`, `USER_GUIDE.template.md` | `rendered/python/`, `rendered/rust/` |

Each template contains one `{{DISTRIBUTION}}` placeholder. The corresponding
`fragments/python/` or `fragments/rust/` document supplies package-specific text. Package trees link
to the generated documents; packaged artifacts contain ordinary files and remain self-contained.

`wrkviz/` is a single-implementation tool, so its `README.md` and `USER_GUIDE.md` are
canonical directly. `dagrun/PLANNER_DESIGN.md` specifies the CPA allocator shared by that tool's
two implementations.

After editing a template, fragment, or single-implementation document, refresh and verify the
outputs:

```sh
python3 scripts/embed_userguides.py
python3 scripts/embed_userguides.py --check
```

`agentctl` keeps its maintained CLI guide, quickstart, and Chat guide directly in
`py/agentctl/`; both implementations embed the same core guide. Its
[RELATED_WORK.md](agentctl/RELATED_WORK.md) compares public open-source systems.
The documentation link check also covers those package-owned assets, without
creating another rendered operator manual here.

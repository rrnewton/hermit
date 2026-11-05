---
title: 'pytest-plugin-identity: keep sharder plugins hashable in pytest'
status: open
priority: 1
issue_type: bug
created_at: 2026-10-11T18:28:28.753920213+00:00
updated_at: 2026-10-11T18:35:10.477824034+00:00
---

# Description

The repository hygiene gate fails on pytest 9.0.3 before the cancellation-control fixture starts. Reproducer: python3 scripts/run_pytest_shard.py --self-test. A captured child traceback shows pytest's FixtureManager.parsefactories raises TypeError: unhashable type: '_ShardPlugin'. The plugin is a mutable dataclass with generated equality, which disables its identity hash; pytest 9 stores registered plugins in a set. This is independent of the deployed web application.

Make the plugin retain object identity for equality/hash, preserving sharding selection and reporting. The existing real SIGTERM fixture must pass without relaxing its descendant-cleanup or timeout assertions. Validate with the repository-selected make validate before publishing. Do not weaken checks or hide the new pytest version behind a downgraded toolchain.


The minimal candidate is changing @dataclass to @dataclass(eq=False) on _ShardPlugin at scripts/run_pytest_shard.py:67, retaining object identity. Applied locally, the existing real SIGTERM self-test passed under pytest 9.0.3 without changes to assertions. Broader make validate passed typing and hygiene but failed five unrelated parser probes tracked as agent-utils-236 (root-terminator-parser); that candidate was then restored out of executable source. The correction remains to be landed with green broader evidence. No executable change is included in the deployment-record commit.


The same cancellation self-test also fails with an isolated pytest 8.4.2 installation. This corrects any impression that the incompatibility is exclusive to pytest 9; downgrading pytest did not provide a workaround.

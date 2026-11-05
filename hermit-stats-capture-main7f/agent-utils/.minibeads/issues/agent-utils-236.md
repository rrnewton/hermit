---
title: 'root-terminator-parser: reconcile CLI guard probes with Python 3.12'
status: open
priority: 2
issue_type: bug
created_at: 2026-10-11T18:32:25.075320367+00:00
updated_at: 2026-10-11T18:32:49.784126639+00:00
---

# Description

Existing repository-infrastructure parser probes fail on Python 3.12.3 at source 002158964114eed96d636490a906b1e771da7236. Reproducer from repository root: python3 -m pytest -q py/tests/test_cross_env_is_hermetic.py -k 'the_parser_probes_include_invocations_that_select_the_installed_herdr or a_root_terminator_does_not_hide_a_subcommand_herdr_from_the_guard'. Five of nine selected cases fail (204 other cases deselected). They expect a root-level -- before a subcommand to select the installed herdr; current argparse returns an invalid COMMAND choice and the parser probe returns None. This also prevents the broader repository-infrastructure validate group completing. Reproduced with unchanged executable source after restoring the separate pytest-plugin identity candidate.

Determine the intended public CLI grammar and fix either its parser or the expectations while retaining the guard against unintended installed-herdr access. Do not simply remove these safety controls. This is separate from vibe-talk: its full application checks and live public deployment verification pass. No CLI parser source or test assertion was changed during the deployment.

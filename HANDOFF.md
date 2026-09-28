# Handoff — agent(review-cpuid)

Written at 97% context. Two live threads: the **phased operational-health cadence** (v6 under
review by hermit-000) and a **routed-away validator finding**. Nothing is committed anywhere.

---

## 1. Interpretation you cannot re-derive from the artifacts

The numbers survive in the task notes. These readings of them do not.

### The phased cadence: four bugs, all caught by my own verification, none by review

The implementation looked right three times and was wrong three times. If you touch it, assume
the same.

1. **A cut-off gate was due on every later tick**, so it leaked onto the other phase — the exact
   pile-up phasing exists to stop. That is why `cadence_window_secs` exists at all. It is not
   decoration; delete it and the feature is pointless.
2. **"Never fired" bypassed the phase entirely**, exempting precisely the gates that need it
   (`wrkslots_audit` ends NO_RESULT every tick and records no epoch). I **reverted** that fix
   after finding `cli.py:435`: the pending report passes an EMPTY fired-state *on purpose* so
   `is_due` calls everything due — that is what makes it a complete inventory. Narrowing
   never-fired would have silently shrunk that report. The accepted residual (a gate that has
   never once completed is offered on both phases) is asserted in a test so it cannot drift.
   **Do not "fix" it again without reading that comment.**
3. **`engine.py:385` calls `is_due` directly, not through `due_reminders`.** I had updated only
   `due_reminders`, so the config was phased, my simulation agreed, and the running tick ignored
   the phase completely — measured 188.3s/185.8s on phase 0 with 34 and 30 gates firing. This is
   the project's one-of-N-consumers shape and I walked into it myself.
4. **My own test expectations were wrong twice.** A phased reminder fires 25 times a day, not 24
   (the first-ever run fires immediately and off-phase before settling). And my first engine test
   used a state where the elapsed and phase rules *happen to agree*, so it caught nothing.

**The transferable lesson:** a test for this feature only discriminates if built on a state where
the two rules disagree. Aligned seeds make elapsed and phased behave identically.

### Why the cross-edition cases had to move, not be patched

The deleted `test_tickhub_cadence_offset_cross_edition.py` called `pytest.skip` when the Rust
binary was absent. The normal order runs Python before cargo builds anything, so **on a clean
checkout it passed by skipping** — reporting success for exactly the one-edition regression it
existed to catch. The cases now live in `cross/differential.py::compare_tick_hub_phased_cadence`,
which resolves the binary through the tracked launcher `rs/bin/tick-hub` and **raises** when it
cannot build. Absence is a failure there, not a quiet pass.

**This is why Python fell 2704 → 2693.** Coverage moved, it did not vanish.

### Two counts that moved for reasons that are not ours

- **2693 → 2703** on the rebase: main's four commits added tests (one is literally "dagrun: Test
  structured-result refusal…"). My test file's sha256 is unchanged across v4/v5/v6. I inferred
  this attribution from the commit set; I did not isolate it by running the base alone.
- **106 did not move** across the rebase. A passing differential prints nothing per case, so the
  count alone proves nothing — I re-ran both discrimination probes on the rebased tree and each
  still produces 2 divergences of 106. That is the evidence the cases still bind, not the count.

### The dev-hermit byte-identity argument (the owner asked me to check it — it holds)

A unified diff embeds the pre-image blob hash in its `index` line. `tick-hub.yaml` is blob
`467325b3d5fdb10891aa79137068e7a6cb8e1645` at **both** old and new bases, so a byte-identical
diff does prove the file did not move. Slight refinement: it proves the *content* is unchanged,
not that no commit touched the path. Independently confirmed with `git diff --name-only` before
v6 was generated, so it does not rest on the inference.

### The routed-away validator finding (mega-lander's lane — do not pick it up)

`the-focused-validator-reports-failed-with-zero-blocking-failures`. **The verdict logic is
correct**; `validate.rs:10078` documents `Some(0)` fatal / `None` non-verdict, and every producer
layer I audited refuses to coerce. The real result: I reproduced a run where the **only** step
was a non-test step with zero libtest markers, and validate still printed `0 test(s) executed…
(aggregated from typed step outcomes)`. **The count does not travel the path its own parenthetical
names.** That is why my first-pass producer audit correctly found nothing — I was tracing a path
the number does not take. Routed; two adjacent observations deliberately left unlinked.

Retained evidence lives at `ignored/review-cpuid/dagrun-evidence-{probe,repro}/` — keep it, the
original incident was unexaminable because the equivalent was discarded.

**Recovery-path caveat worth knowing generally:** `RunEvidence::open` (dagrun `attribution.rs:244`)
documents "evidence capture must never be able to fail a run" and silently disables per-step logs
**and attribution** with only a warning if the directory is missing, not a directory, or not
private. A mis-specified evidence directory degrades to no evidence while the run still reports.

---

## 2. The v6 snapshots (under review by hermit-000)

Both **uncommitted**. Both bases verified `== origin/main` at generation. Worktrees untouched.

| | agent-utils | dev-hermit |
|---|---|---|
| worktree | `/tmp/review-cpuid-au` | `/tmp/review-cpuid-dh` |
| repository | `…/hermit/.git/modules/agent-utils` | `/home/newton/work/dev-hermit` |
| base SHA | `250c36997fa7a550d4cf1c9cf2802b76badb9393` | `cb999443c01a564c2d958274426b105b63f053b2` |
| head | uncommitted | uncommitted |
| diff | `/tmp/review-cpuid-au-v6.diff`, 950 lines | `/tmp/review-cpuid-dh-v6.diff`, 344 lines |
| sha256 | `cb252d3a31a80b9edfbb6cabeb61e93885113f9d8b4d906120a719f22d8e20ce` | `c7ab4c525538ac0ebe4b8578936df62b9bb8fdc71c0f98a554635d6d153179c5` |

New file (agent-utils, untracked, **not in the diff** — `git diff` omits untracked paths, so a
reviewer reading only the diff misses all 18 tests):
`py/tests/test_tickhub_cadence_offset.py`, 308 lines,
sha256 `25f76f3e217ca4d781a0e5a7abbff2b064d2b599d97cacb425320717d6e8e3eb`

Changed paths — agent-utils (12): `cross/differential.py`;
`py/tick_hub/{cadence,engine,io,model}.py`;
`rs/tick-hub/src/{cadence,engine,io,model,probes,protocols}.rs`; the new test.
dev-hermit (1): `ci-hub/health/tick-hub.yaml` — **byte-identical across v2–v6**.

Copies under `ignored/review-cpuid/phased-cadence-*-v6.diff` (gitignored, not the authority).

Mandatory set on the rebased tree, status captured before reading output: `cargo fmt --check`
rc=0; `mypy --strict` rc=0 (317 files); differential rc=0 (106); `pytest` rc=0 (2703);
`cargo test --workspace` rc=0 (871).

---

## 3. The Hermit gitlink bump — owner has ruled: SPLIT IT

**Do the catch-up bump first, separately, and now** — it does not depend on the cadence change
landing, so it can run in parallel with the v6 review instead of queueing behind it.

- hermit `origin/main` pins agent-utils at **`e5074d1026ef92ee8bc9d42d44e9d18625bae225`**
- agent-utils `origin/main` is **`250c36997fa7a550d4cf1c9cf2802b76badb9393`** — already **4
  commits** ahead before the cadence change exists

The four in the range:

```
250c369  gent-talk: add a channel from inside the app, and group the per-channel settings
7ebb947  dagrun: Test structured-result refusal with the typed outcome shape
4c65b3b  dagrun: preserve explicit no-result declarations
fb90f1b  dagrun: declare structured test result producers
```

**Owner's reasoning, recorded so it is not lost:** bundling makes the cadence review harder, not
easier; and gent-talk is unmeasured and unrelated, so folding it into a change that has taken six
rounds to validate would contaminate that validation with a commit nobody has examined. The three
dagrun commits are **already measured** against the differential cases (see §1), so the catch-up
review is genuinely small — gent-talk is the only unexamined part.

**Constraints on whoever takes it:**

- A pin bump **imports everything in the range**. This is a five-commit review across two
  unrelated areas if bundled — which is why it is being split.
- I verified authority exists to open it: `git push --dry-run` to hermit returns rc=0 for a new
  branch.
- **Separation of duties blocks self-landing.** `ci-hub/bin/review-attest:278` refuses when
  `--who` equals `--reviewer`. The bump needs an independent review lane **and** a separate
  attester, exactly as hermit PR 2950 did. Whoever writes it cannot attest it.
- The cadence bump additionally waits on the landed agent-utils sha, which does not exist yet.

**Landing order, unchanged:** agent-utils → hermit gitlink bump → dev-hermit. Reverse order is an
outage, not a degradation: before agent-utils lands, the shipped parser **refuses** the phased
config outright (`unknown field(s): cadence_offset_secs, cadence_window_secs`) and the tick parses
nothing. Safe in one direction only — the new parser accepts an unphased config, so agent-utils
may land early and sit idle.

---

## 4. State on resume

- Do **not** start the bump and do **not** respond to v6 findings without re-reading the task
  notes on `operational-health-workflow-intermittently-exceeds-180-second-budget`; every round's
  evidence is recorded there verbatim.
- If a base moved again, that is visible by re-reading `origin/main` against the two base SHAs in
  §2. It does not silently invalidate the diffs, but it needs the same rebase-and-re-verify
  treatment v6 got — and the base **will** move: agent-utils gained four commits inside one
  review pass.
- Closed on the way here: `rust_tick_hub_gate` (Rust gate `timeout_secs`, implemented and
  enforced). Filed and not mine: `final_validate_status_has`,
  `unknown-typed-rows-message-cannot-distinguish-unconfigured-from-failed`,
  `tick-hub-output-omits-per-gate-wall-duration`.

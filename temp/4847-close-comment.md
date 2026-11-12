Closing as superseded: this change's functionality is already on
`main` (verified against `5c4cbb15fa23ff7b7e6aa6e3bab5463acc87d413`),
in evolved form, through other landed work. The branch tip
`aa89b2215750fdc5cbfe1f6103d310cebec530e7` is preserved as
branch `rescue/20261105-pr4847-aa89b22157`; nothing here was deleted.

Feature mapping to covering commits on main:

- Store where each backgrounded call rejoined in the preemption record, and
  replay at those turns: `f83631f1139c63190c962cd1ee3a615b9ee8da51`.
- Readmit by the recorded turn's timed pops in a schedule replay:
  `0d4303c9d19ad3bd08e6fb6b0d0c448d81e8a22f`.
- Begin a backgrounded call where its recording did in a schedule replay:
  `5e4931428f8fded9d364c3031c0f042caf2fef6f`.
- Background-starts record format (statement and numbering):
  `67f96e593f8005cfd4b1e1b4b7c558458005f20a`, with preemption format 5 in
  `250fadf01a6efb9f5f45840302e8391f38e6865f`.
- Leaving a preemption record at the nothing-can-run valve:
  `d8f3d42cc46e30f520723d65f34f16671ed20a0a`; empty-readmission handling and
  strict/die-on-desync leaving: `329c68395eff953e7d2706eef3e0b2a51b8f4d02`.
- Binder background path and post-grant receipt: the landed binder series,
  tip `306e9b8a6a38940cd6c219cc01c237d53d2d41b1`.

The only tests in this branch not present on main encode semantics that
main deliberately replaced: they expect the gated/exhausted rejoin valves
to leave the record and readmit by host timing. Main instead refuses at
the valve (`refuse_rejoin_at_the_valve`), with renamed tests asserting
that refusal, so the two branch tests fail on main by design and were
not ported.

— musetrain (drain lane), per coordinator-approved scope audit

# Composable complete-record layers

The additive record_layer_with_failure API is implemented and ready for root review. The change is uncommitted. Earlier independent approval applies only to immutable 944138ef2b3044cd4576a7d1388075d7260b55e7; I authored this new change and am not independently approving it.

Write destination: /home/newton/work/dev-hermit/worktrees/slots/codex-liteinst-resume-01a0a13c, branch codex/liteinst-resume-01a0a13c. Base HEAD remains 944138ef2b3044cd4576a7d1388075d7260b55e7. Only hermit-cli/src/liteinst_record.rs and hermit-cli/src/liteinst_record/tests.rs have tracked changes. The old slot and shared hermit checkout were not modified. No transport, production tracing wiring, filter-policy change, comparator, dependency, selection, skill or protocol change is included.

The new constructor returns an uninstalled, unfiltered Layer and its retained RecordStatus. Callers apply Layer::with_filter independently, allowing the public EnvFilter and private fixed INFO selector to coexist. The existing record_subscriber_with_failure signature and behavior remain, with both constructors using the same internal layer builder.

Each layer owns an Arc identity. A span extension holds a separate FormattedFields<CheckedFields> entry for each identity. Entries retain that Arc, so an entry cannot accidentally match a reused allocation after its original layer is dropped. Lookup never sorts by address or exposes identity in records. Each update formats only that layer's cache using its own limit and failure state. Replacing the old shared extension key prevents both duplicate insertion and cross-layer field updates. The small cache-entry Vec is registry bookkeeping, not a newly claimed bound on process memory; the existing payload budgets and their documented limits are unchanged.

The first new regression was run after exposing the Layer API but before changing the old shared FormattedFields extension. It failed on the second layer's insertion at tracing-subscriber Extensions::insert, Cargo exit 101 after 36.474 seconds including the fresh build. Both source hashes and the negative patch are retained in target/liteinst-layer/shared-cache-negative.json and shared-cache-negative.patch; the full output is shared-cache-negative.log. This was an executed test failure, not a compilation-only or hypothetical control.

The strict passing test uses independent stock subscribers for the public and private output oracles. Two stock fmt layers themselves share the field-type cache, so making that composition the sole oracle could copy duplicate updates into the expected output. With independent oracles, the candidate must produce all seven public and all eight private records byte-for-byte, with additional exact literal records checking selection, span updates and normalized log-bridge metadata. Cases include private INFO outside the dynamic public filter, public TRACE, nested spans with differing layer visibility, current and explicit parents, parent None, ancestor updates, dynamic field matching and a real log bridge callsite.

Two further tests exercise the public constructor and failure independence. A public sink returning EPIPE is attempted once and notifies once; the private layer retains both records, its one span update and a clear status. A three-byte span cache overflows on update while the other layer's 64-byte cache remains complete, tested with the failing layer in either composition position. The accepted first record and exact full second record remain asserted. The real-time public-constructor smoke test checks complete record suffixes and callback/status behavior; the independent stock test uses the common fixed input timer for full-byte comparison.

All 47 original logger tests remain; three were added. The old single-layer cache inspection/removal helpers now inspect the per-layer container and require exactly one entry, preserving all their original field, capacity and allocation assertions. I verified the complete source tail beginning with timer_failure_preserves_stock_fallback_but_cannot_hide_overflow is byte-identical to base, including the TLS subprocess helper, full 142-byte positives, stock failure controls and known abort/124-byte defect characterizations. No assertion, acceptance threshold or skip was weakened.

## Source binding

| Path | Bytes | SHA-256 |
| --- | ---: | --- |
| hermit-cli/src/liteinst_record.rs | 22236 | 5bd8d575a0c7d799872ab003df2283a6e9c3c027889d2df4079f5c77ceb3ea7b |
| hermit-cli/src/liteinst_record/tests.rs | 64913 | ea1a5afcd8ce40d82aff335a4f7a86b0980e4148eb75de2a3f37e1172d1b38ab |

The full diff is target/liteinst-layer/implementation.patch, 18,022 bytes, SHA-256 057c6b1adc2aec19b3ad4a45fc2c6df644de85b14ee6d5a535605ffca65354ee. The tracked diff has two paths, 326 additions and 27 deletions. Source manifest: target/liteinst-layer/source-files.json. Source hashes remained unchanged throughout final checks and were checked again afterward.

## Executed checks

| Command | Result | Wall time |
| --- | --- | ---: |
| with-proxy cargo test -p hermit --lib liteinst_record::tests:: | 50 passed; zero failed/ignored; 260 filtered | 12.163 s |
| with-proxy cargo test -p hermit --lib | 310 passed; zero failed/ignored/filtered | 1.001 s |
| with-proxy cargo clippy -p hermit --lib --tests -- -D warnings | exit 0 | 21.515 s |
| cargo fmt -p hermit -- --check | exit 0 | 0.966 s |
| git diff --check | exit 0 | 0.040 s |

These are native library checks. The counts include subprocess helper entries and known-defect characterization tests; they are not backend cells or complete-log successes. Full command/source/result records are in target/liteinst-layer/layer-tests-1.json and final-checks.json, with corresponding logs. One intermediate cache repair compile attempt used a nonexistent ExtensionsMut::get method and exited 101; it is retained as independent-cache-positive.json/.log and was corrected to get_mut before the passing runs. That filename is historical; its recorded outcome is a compilation failure, not a positive execution.

No whole-workspace or canonical admission run, guest invocation, backend determinism measurement or parity comparison was performed. Registry/EnvFilter TLS lifecycle defects remain. The existing span-update panic contract still follows the underlying Registry lock's poisoning behavior; this change does not isolate a propagated panic from a shared Registry. The new independence controls establish ordinary returned sink/size failures and separate successful field updates. Payload limits remain provisional. Capture transport, ordering, failure-to-comparator propagation and emitter completion remain future integration obligations. Root must review the complete diff, arrange fresh independent review as required, and decide the commit/validation sequence.

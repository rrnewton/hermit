# Separate initialized-VM and guest qualification

Prepared source only, not part of the 60 Hermit native controls or 34 Reverie native controls. No VM was constructed and no guest ran while preparing these files. Review and an actual official selector/runner integration are still required before execution.

setup_failure.rs is a concrete proposed Hermit CLI control using the actual public KVM completion path after VM creation and ELF installation. Its Tool owns real Detcore ThreadState and GlobalState and deliberately returns a typed I/O error from the first start callback before Detcore registration or guest execution. Both consuming hooks delegate to real Detcore; the thread hook then returns a separate typed cleanup cause. Exact construction/start/thread/process counts, retained primary and cleanup errors, zero recorded vCPU exits, and natural GlobalState cleanup are required. The fixture's UD2 image ensures an accidentally started guest cannot return the requested result. This establishes initialized-VM start-hook setup failure; it does not inject or claim a measured failure of earlier track_clock/get_regs/scratch-map preamble operations. A dedicated earlier-preamble returned-error control remains required if final coverage claims include those producers.

Existing Reverie selectors required alongside it:

- vm::tests::nested_host_fork_failure_uses_descendant_process_and_worker_identity: two actual shared fork preparations, then an owned host-thread error; requires KVM construction/snapshot/register ioctls, no vCPU run.
- vm::tests::real_fork_and_thread_wrappers_restore_both_capture_modes: unchanged real fork/thread cancellation and continuation assertions. This uses actual VM instructions in its established setup.
- vm::tests::page_fault_action_restores_complete_stopped_context: unchanged captured page-fault and ordinary unstarted-child cancellation assertions, with actual vCPU execution.

Set REVERIE_REQUIRE_KVM=1 when measuring those existing selectors so their historical unavailable-KVM early return cannot appear as coverage. Keep full raw outcomes and classify a setup refusal before comparison as unmeasured/incomplete, not a guest mismatch or successful test.

A subsequent actual strict guest run must use the original strict fork-tree fixture and the existing official validation/E2E entry, with exact source, executable, argv, environment, working directory, stdout, stderr, exit status and first-attempt/retry provenance. Preserve its current comparator and every bound. A returned-error repair does not implement virtual SIGCHLD or establish that the 49 census first-guest rejections share this cause. Root owns the original fixture/trace and cohort identity, so this lane must bind that retained fixture rather than substitute a newly invented fork program. No guest command has been released or prepared as executable authority here.

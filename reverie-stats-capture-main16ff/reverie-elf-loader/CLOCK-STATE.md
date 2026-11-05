# Inactive LB7 clock-state fixture

`ClockStateCarrier` keeps the runtime RCB offset and opaque Tool state in a
sealed memfd. Its format does not define logical time: that state belongs to
the Tool and must be restored into the Tool's actual owner.

## Owner restoration and reads

The fixture in `tests/proc_state.rs` uses an ordinary Reverie Tool whose
`ThreadState` is `[committed logical nanoseconds, last committed RCB count]`.
Before transfer, the Tool runs through `ToolHost::dispatch`, commits seven
new guest RCBs, and reads both saved fields through `Guest::thread_state`.
Its committed logical time is 42,731,099 ns and its committed RCB boundary
is 19,308.

The native successor restores the runtime counter with `restore_rcb_clock`
under a different raw counter origin (50,491). It installs the saved Tool
state using `ToolHost::restore_current_thread_state`. That small input seam
places the state in the host's existing per-thread state map. It accepts only
a thread with no existing state, does not repeat the already-completed
thread-start or post-exec callbacks, and refuses an active dispatch or a second
restoration. Allocations use the existing private Tool allocation scope.

The successor reads logical time through `ToolHost::dispatch` and the fixture
Tool's `Guest::thread_state` access. Its commit callback reads the actual
runtime clock with `Guest::read_clock`, subtracts the saved committed RCB
boundary, updates logical time through `Guest::thread_state_mut`, and retains
the new committed boundary. This follows Detcore's ownership pattern; the
fixture uses 10 ns per RCB and keeps the logical value separate from the raw
counter origin. Reading the runtime counter alone does not commit Tool time.

## Sensitivity controls

- Zeroing the carrier's counter offset fails the existing complete RCB
  trajectory assertion, including its nested-handler deductions.
- Zeroing only the carrier's logical value preserves the counter offset and
  committed RCB boundary, passes that RCB assertion, then fails the same owner
  logical-time trajectory assertion used by the positive successor.
- Restoring over either previously restored or normally initialized Tool
  state returns `AlreadyExists`; reads still observe the previous owner state.
- Logical-time progression checks individual RCB increments and both active
  and completed nested handlers. The expected values come from the saved
  accounting boundary and known guest increments, not decoded logical bytes.

## Scope

The restoration interface has no production caller. The fixture exercises the
actual Reverie counter, ToolHost state owner, dispatch and Guest access paths
with controlled perf metadata and a fixture Tool. It does not restore Detcore's
complete state, qualify hardware PMU delivery, or qualify a Hermit mode. No
production exec path consumes this carrier, and the in-guest exec gate remains
unchanged.

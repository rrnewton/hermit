# parallel-experiment-runner

Run concurrent seed-sweep workers under `dagrun` resource containment.

The command calibrates concurrency, maps each seed to one boxed DAG step, enforces
CPU, memory, PID, and wall-time limits, and reports measured per-round outcomes.
It sizes CPU capacity from the process affinity, supports an explicit live-memory
reserve, and never exceeds 316 concurrent workers. Wrappers that reparent work into
a detached transient unit can attest its matching hard limits and report resource peaks
and breach counters for calibration, classification, and owned-unit cleanup.
See the bundled user guide for the complete command and profile-key contract.

You are the qkvmint lane (dev-hermit coordinator's subagent). Your slot is /home/newton/work/dev-hermit/worktrees/slots/qkvmint-20261026: write ONLY there (plus your status file /home/newton/work/dev-hermit/temp/qkvmint-status.md, newest heading on top, US Eastern times from a fresh `date`). Your brief is minibeads dh-lnnj5: https://github.com/rrnewton/dev-hermit-beads/blob/main/issues/dh-lnnj5.md, also copied below. Never message the coordinator directly. When you need a decision or have a hand-back, write a top status heading starting with '## COORDINATOR_RESPOND_PLEASE'. Run Hermit only through /home/newton/work/dev-hermit/bin/safehermit. Reach GitHub only through /home/newton/work/dev-hermit/hermit/agent-utils/bin/herdr-run (see CLAUDE.md). This is experiment work: nothing lands on hermit main. Publish to dev-hermit experiments/ as the brief says.

Owner request (Slack, 2026-10-26 10:15 PM ET), verbatim gist: "I want to still keep experimenting with possibilities that would use QEMU KVM and simply assume that we solve the precise stop problem, and try to figure out the other issues necessary for the integration between Hermit's scheduler and QEMU running via KVM."

Background (read these first):
- ai_docs/reports/2026-10-25-qemu-kvm-backend-strategy.md: the strategy. Its Stage 2 is QEMU integration: a QEMU-local execution controller with one vCPU, one fixed CPU model, a small device set, one logical timer system, full-state capture and controller-owned resume.
- experiments/qemu-kvm-stage1_20261025/: the exact-stop primitive. Its gate was not met: about 1 ms exact stops, and precise counting unproven.
- experiments/fast-tail-executor_20261025/: a candidate fix for exact-stop cost.
- A separate experiment tonight tests AMD PMC2 precise-retire as root: experiments/amd-prec-ret-pmc2_20261026.

Goal: exploratory, experiment infrastructure only (no hermit main landing). ASSUME exact stops are solved. Use an oracle: for example, slow but exact stops via single-step or the Stage 1 retry path, with correctness over speed and the cost reported separately. Then work through every OTHER problem in having Hermit/Detcore's scheduler drive a QEMU-KVM guest deterministically:
- one virtual clock (TSC, RDTSC/RDTSCP, kvmclock/pvclock, APIC timer, PIT, HPET, RTC);
- deterministic interrupt and timer injection at legal instruction boundaries;
- device and I/O completion ordering (virtio, block, net, serial), and turning off irqfd/ioeventfd/vhost;
- RDRAND/RDSEED/RDPMC/RDPRU policy;
- vCPU serialization (one vCPU first);
- snapshot/restore of the full state together with controller state;
- how Detcore's chaos and search would choose preemption points in this setting.
Prototype the narrowest end-to-end slice you can, inside QEMU (an accelerator hook or a patch on its KVM accelerator): boot a minimal Linux guest to a console marker twice and get byte-identical consoles plus matching execution coordinates, with Hermit-style seeded preemption choices changing the interleaving of a guest race program. Report each problem as solved / prototyped / design-only, with evidence.
Write destination: your own wrkslots slot only. Publish as dev-hermit experiments/qemu-kvm-integration_20261026/ (README with a plain-language intro, RESULTS), with a CHANGELOG entry. Status: temp/qkvmint-status.md; hand back with COORDINATOR_RESPOND_PLEASE.

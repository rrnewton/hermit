# Dependency-aware targets for the Hermit demo suite.
#
# Process demos share Hermit's build prerequisites. QEMU demos share provisioned
# kernel/initramfs inputs, and demos 6/7 consume the phase-5 boot snapshot. Keep
# the snapshot as a real file target: if it exists, consumers restore it without
# rebooting Linux; if it is absent, Make runs demo 5 exactly once to produce it.

DEMO_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(DEMO_DIR)..)
CLEAN := $(DEMO_DIR)clean.sh
PYTHON ?= python3
QEMU_ASSETS ?= $(ROOT)/ignored/qemu-linux
BOOT_SNAPSHOT := $(QEMU_ASSETS)/hermit-boot.qcow2
DEMO6_COMMAND ?= uname -a
PROCESS_DEMOS := demo1 demo2 demo3 demo4
QEMU_DEMOS := demo5 demo6
DEMO_TARGETS := $(PROCESS_DEMOS) $(QEMU_DEMOS) demo7

.PHONY: $(DEMO_TARGETS) all clean distclean help process-prereqs \
	qemu-prereqs drgn-prereqs test

help:
	@echo "make demo1 .. demo4  Run one process-level demo."
	@echo "make demo5           Ensure the reusable Linux boot snapshot exists."
	@echo "make demo6           Restore it and run DEMO6_COMMAND='uname -a'."
	@echo "make demo7           Ensure demo5's snapshot exists, then run drgn."
	@echo "make all             Run every checked-in demo (1-7) in order."
	@echo "make clean           Wipe computed results, including the snapshot."
	@echo "make distclean       Also remove downloaded kernel/initramfs blobs."
	@echo "make test            Run fast, hermetic unit tests (no QEMU)."

# Fast dependency checks are order-only prerequisites. The demo entry points
# retain their incremental build/provisioning logic, so direct script execution
# and Make execution satisfy the same requirements without timestamp stamps.
process-prereqs:
	@$(MAKE) -C "$(ROOT)" --no-print-directory init-hermit check-deps

qemu-prereqs: process-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" "$(DEMO_DIR)lib/qemu-assets.sh" --check

drgn-prereqs: qemu-prereqs
	@command -v drgn >/dev/null 2>&1 || { \
		echo "ERROR: drgn is required for demo 7." >&2; exit 1; }
	@command -v readelf >/dev/null 2>&1 || { \
		echo "ERROR: readelf is required for demo 7." >&2; exit 1; }

demo1: | process-prereqs
	@"$(DEMO_DIR)01-deterministic-run.sh"

demo2: | process-prereqs
	@"$(DEMO_DIR)02-record-replay.sh"

demo3: | process-prereqs
	@"$(DEMO_DIR)03-chaos-concurrency.sh"

demo4: | process-prereqs
	@"$(DEMO_DIR)04-schedule-bisection.sh"

# Artifact edge shared by demos 5-7. Demo 5 publishes this path atomically.
# The postcondition catches misconfigured QEMU_ASSETS or an incomplete boot.
$(BOOT_SNAPSHOT): | qemu-prereqs
	@echo "Phase-5 snapshot missing; running demo 5 prerequisite..."
	@QEMU_ASSETS="$(QEMU_ASSETS)" "$(PYTHON)" "$(DEMO_DIR)05-qemu-boot.py"
	@test -r "$@" || { echo "ERROR: demo 5 did not produce $@" >&2; exit 1; }

demo5: $(BOOT_SNAPSHOT)
	@echo "Demo 5 prerequisite ready: $(BOOT_SNAPSHOT)"

demo6: $(BOOT_SNAPSHOT) | qemu-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" \
		QEMU_BOOT_SNAPSHOT_DISK="$(BOOT_SNAPSHOT)" \
		"$(PYTHON)" "$(DEMO_DIR)06-qemu-resume.py" $(DEMO6_COMMAND)

demo7: $(BOOT_SNAPSHOT) | drgn-prereqs
	@QEMU_ASSETS="$(QEMU_ASSETS)" \
		DEMO07_SNAPSHOT_DISK="$(BOOT_SNAPSHOT)" \
		"$(DEMO_DIR)07-drgn-kernel.sh"

# Recursive Make preserves the declared order even when the caller uses -j.
all:
	@set -e; for demo in $(DEMO_TARGETS); do \
		$(MAKE) --no-print-directory "$$demo"; \
	done

test:
	@$(PYTHON) -m unittest discover -s "$(DEMO_DIR)tests" -p 'test_*.py' -v

clean:
	@"$(CLEAN)"

distclean:
	@"$(CLEAN)" --distclean

# Convenience targets for the Hermit QEMU/Linux demo suite.
#
# The demos persist a first-run "anchor" (run-metadata.json) and compare later
# runs against it; a stale anchor produces a false PARTIAL after code changes.
# These targets give a one-command fresh start. See clean.sh for details.

DEMO_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
CLEAN := $(DEMO_DIR)clean.sh

.PHONY: clean distclean help

help:
	@echo "make clean      Wipe computed demo results (anchors, run history,"
	@echo "                snapshots) so the next run starts fresh. Keeps the"
	@echo "                downloaded kernel and built initramfs."
	@echo "make distclean  Also remove the kernel download and initramfs blobs."

clean:
	@"$(CLEAN)"

distclean:
	@"$(CLEAN)" --distclean

SHELL := /bin/sh
BUILD_DIR ?= build
WORKER ?= ./make_worker
UNITS := 0 1 2 3
STAMPS := $(addprefix $(BUILD_DIR)/unit,$(addsuffix .stamp,$(UNITS)))

.PHONY: all
all: $(STAMPS)
	@printf 'stamps='
	@sha256sum $(STAMPS) | awk '{ printf "%s%s", separator, $$1; separator="," } END { print "" }'

$(BUILD_DIR):
	@mkdir -p $@

$(BUILD_DIR)/unit%.stamp: | $(BUILD_DIR)
	@$(WORKER) $* > $@
	@cat $@


# This Makefile is only here to collect commonly run commands, as a kind of shortcut or "cheat sheet".

# Defaults:

default: build run
build: docker-build
run: docker-run

# Docker
# ------------------------------------------------------------

# Run an existing binary from Dockerhub:
quickstart:
	docker run -it --rm rrnewton/hermit

DOCKERTAG_DEV=hermit-dev
DOCKERTAG_RELEASE=hermit
DOCKERPLATFORM=linux/amd64

docker-build:
	docker build -f oss/Dockerfile.dev -t $(DOCKERTAG_DEV) . --platform $(DOCKERPLATFORM)

docker-release: docker-build
	docker build -f oss/Dockerfile.deploy -t $(DOCKERTAG_RELEASE) . --platform $(DOCKERPLATFORM)

docker-run:
	docker run --volume `pwd`:/build --workdir=/build -it --rm --platform $(DOCKERPLATFORM) $(DOCKERTAG_DEV)


# Nix:
# ------------------------------------------------------------

# TODO


# Docker+Nix:
# ------------------------------------------------------------

# TODO


# Buck2: Meta-internal until it is ported to work externally.
# ------------------------------------------------------------

buck2-build:
	buck2 build hermit-cli:hermit

# A local validation run for hermit:
buck2-test:
	buck2 test @//mode/dev-nosan detcore/... hermit-cli: tests/... -- --timeout=200 --retry=5


.PHONY: default build run quickstart docker-build docker-run buck2-build buck2-test

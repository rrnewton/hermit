# Shared repository assets

This directory contains sources shared by the repository's independently installable tools.

- `docs/` holds canonical documentation sources and generated package documents.
- `bin/` holds source-checkout dispatchers and their small engine-selection helpers.

The dispatchers are a development convenience, not package entry points. Run `./setup` at the
repository root to create `./bin`, or use the commands installed by an individual package.

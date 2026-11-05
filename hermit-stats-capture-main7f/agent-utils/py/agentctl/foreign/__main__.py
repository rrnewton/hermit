"""Run the persistent foreign-worker command dispatcher as a module."""

from .cli import main

raise SystemExit(main())

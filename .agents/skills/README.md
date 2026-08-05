# Codex skill entrypoints

Stock Codex discovers the product skills in this directory. Flat canonical
Claude skills use small `SKILL.md` entrypoints; already packaged skills are
directory symlinks back to `.claude/skills/`. The canonical bodies remain in
one place, and `.llms/skills` remains the Claude/LLMS compatibility link.

Run `scripts/check-skill-discovery.rs` after changing product skills. Parent
coordinator roles do not belong in this product repository.

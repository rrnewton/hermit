# agentctl inbox quickstart

A coordinator that supervises several workers needs to hear when one stops, gets stuck, or
reports something, without polling every worker and without several writers typing into its
session at once. `agentctl inbox` keeps those notices in a durable queue and hands them over as
one prioritized batch.

```bash
# A worker (or a watcher acting for it) reports.
agentctl inbox post --to coord --from reviewer --kind idle --text 'Review done: 2 findings in the PR'
agentctl inbox post --to coord --from builder --kind blocked --text 'Needs approval to delete the cache'

# See what would be delivered, in order: blocked and exited first, then idle and messages.
agentctl inbox render --to coord

# Or let a watcher post idle, blocked, exited and reminder notices for every Herdr worker.
agentctl inbox watch --to coord --once

# Deliver: print for a cron or loop to read, or notify an agentcloud session exactly once.
agentctl inbox deliver --to coord --via print
agentctl inbox deliver --to coord --via agentcloud-notify --session SESSION_ID
```

A worker keeps one live state notice; a newer one replaces it. `--kind working` withdraws it.
Explicit messages (`--kind message`) are never merged. Run `agentctl inbox userguide` for the
full reference.

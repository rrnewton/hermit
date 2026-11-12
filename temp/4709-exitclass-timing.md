Timing rule from coordinator (15:2x ET), reinforcing your brief:
- 4709 is NOW validating ALONE as queue v21 on base 5bb83135 (published 2:57 PM ET).
- If you prove cli.rs:50846 Some(122) is 4709's own site, v21 will be red on check.exit_status_class anyway: push the fix (declare the exit channel, no baseline raise) as soon as it is ready and report the 40-hex head immediately; coordinator resubmits.
- If the site is NOT 4709's, do NOT push anything — a push while v21 is in flight would throw away a possible green. Report the attribution proof instead.
- image_archive.rs sites belong to 5186 (musere is fixing those); never touch them.
Prioritize the per-tree checker runs (fresh main / 4709 head) so the attribution lands fast; the fresh-main composition is secondary if (a) vs (b) already decides ownership of the 13th site. Note: v21's base is 5bb83135 — if the checker passes at 4709's old base but the site is 4709-introduced relative to ANY base, it is still 4709's to fix; say exactly which base each run used.

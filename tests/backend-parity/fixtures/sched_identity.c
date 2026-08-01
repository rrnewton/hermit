/*
 * Backend-parity fixture: deterministic scheduler identity and nice policy.
 *
 * Hermit serializes guest threads onto one deterministic scheduler, so a
 * guest's scheduling attributes are fixed and host-independent. A guest thread
 * always reports the SCHED_OTHER policy with static priority 0, and SCHED_OTHER
 * has no static-priority range (max == min == 0).
 *
 * The nice value is a scheduling input that would perturb timing, so Hermit
 * determinizes it away: setpriority(2) is accepted (returns 0) but the process
 * nice value stays 0 under every backend. Outside Hermit the same setpriority
 * call lowers the priority (getpriority would then report 10), so the fixed nice
 * readback is a determinization choice, not native parity -- the same shape as
 * the membarrier query-mask normalization.
 *
 * All assertions are process-local and carry no host-derived value, so the
 * output (`sched ok=6`) is identical across runs, backends, and hosts.
 */

#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <sys/resource.h>
#include <unistd.h>

int main(void)
{
	int ok = 0;

	/* A guest thread runs under SCHED_OTHER. */
	if (sched_getscheduler(0) == SCHED_OTHER)
		ok += 1;

	/* SCHED_OTHER carries static priority 0. */
	struct sched_param param;
	param.sched_priority = -1;
	if (sched_getparam(0, &param) == 0 && param.sched_priority == 0)
		ok += 1;

	/* SCHED_OTHER has no static-priority range. */
	if (sched_get_priority_max(SCHED_OTHER) == 0 &&
	    sched_get_priority_min(SCHED_OTHER) == 0)
		ok += 1;

	/* A fresh guest process starts at nice 0. */
	errno = 0;
	int nice_before = getpriority(PRIO_PROCESS, 0);
	if (nice_before == 0 && errno == 0)
		ok += 1;

	/* setpriority is accepted... */
	if (setpriority(PRIO_PROCESS, 0, 10) == 0)
		ok += 1;

	/* ...but Hermit determinizes the nice value, so it stays 0
	 * (native would report 10 here). */
	errno = 0;
	int nice_after = getpriority(PRIO_PROCESS, 0);
	if (nice_after == 0 && errno == 0)
		ok += 1;

	printf("sched ok=%d\n", ok);
	return 0;
}

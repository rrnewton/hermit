/*
 * Backend-parity fixture: deterministic sched_getattr/sched_setattr identity.
 *
 * sched_getattr(2)/sched_setattr(2) are the modern extended scheduler-attribute
 * interface: a single sched_attr struct carries the scheduling policy, nice
 * value, real-time priority, and deadline-scheduler parameters. They are
 * distinct syscalls from the classic sched_getscheduler/sched_getparam/
 * getpriority/setpriority getters exercised by sched_identity; this fixture
 * probes the extended pair directly.
 *
 * Under Hermit every thread runs on one deterministic logical CPU, so the
 * scheduler attributes must be a fixed process-local identity rather than a
 * host-derived value: policy SCHED_OTHER (0), nice 0, and real-time priority 0.
 * A niceness change is accepted (sched_setattr returns 0, faithful Linux
 * behavior for an in-range request) but does not perturb the deterministic
 * readback -- a later sched_getattr still reports nice 0.
 *
 * The discriminator is that niceness readback: outside Hermit, sched_setattr
 * with sched_nice=7 makes the next sched_getattr report nice=7, so native
 * prints `sched_attr ok=4`. All three Hermit backends hold the determinized
 * nice=0, printing `sched_attr ok=5`. The uniform Hermit result is therefore a
 * determinization choice, not native parity. All assertions are process-local
 * and the output is identical across runs, backends, and hosts.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_sched_getattr
#define SYS_sched_getattr 315
#endif
#ifndef SYS_sched_setattr
#define SYS_sched_setattr 314
#endif

#define SCHED_OTHER_POLICY 0

/* Kernel sched_attr layout (see linux/sched/types.h). */
struct sched_attr_k {
	uint32_t size;
	uint32_t sched_policy;
	uint64_t sched_flags;
	int32_t sched_nice;
	uint32_t sched_priority;
	uint64_t sched_runtime;
	uint64_t sched_deadline;
	uint64_t sched_period;
};

static long getattr(struct sched_attr_k *a)
{
	memset(a, 0, sizeof(*a));
	errno = 0;
	return syscall(SYS_sched_getattr, 0, a, (unsigned)sizeof(*a), 0u);
}

static long setattr_nice(int32_t nice)
{
	struct sched_attr_k s;
	memset(&s, 0, sizeof(s));
	s.size = sizeof(s);
	s.sched_policy = SCHED_OTHER_POLICY;
	s.sched_nice = nice;
	errno = 0;
	return syscall(SYS_sched_setattr, 0, &s, 0u);
}

int main(void)
{
	int ok = 0;
	struct sched_attr_k a;

	/* sched_getattr is accepted and reports the SCHED_OTHER policy. */
	if (getattr(&a) == 0)
		ok += 1;
	if (a.sched_policy == SCHED_OTHER_POLICY)
		ok += 1;

	/* Deterministic identity: nice 0 (and real-time priority 0). */
	if (a.sched_nice == 0 && a.sched_priority == 0)
		ok += 1;

	/* An in-range niceness change is accepted, faithful to Linux. */
	if (setattr_nice(7) == 0)
		ok += 1;

	/* Determinized readback ignores the change: still nice 0.
	 * Native honors the set here (nice=7) and misses this check. */
	if (getattr(&a) == 0 && a.sched_nice == 0)
		ok += 1;

	printf("sched_attr ok=%d\n", ok);
	return 0;
}

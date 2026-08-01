/*
 * Backend-parity fixture: deterministic I/O-scheduling priority policy.
 *
 * The I/O priority (ioprio) is a scheduling input, the block-layer analogue of
 * the nice value. Honoring it would make block-request ordering depend on host
 * I/O-scheduler state, so Hermit determinizes it away: ioprio_set(2) is accepted
 * (returns 0) but ioprio_get(2) always reports 0 (IOPRIO_CLASS_NONE, priority 0)
 * afterward under every backend. Outside Hermit the same ioprio_set call takes
 * effect, so ioprio_get then returns the value that was set. The fixed readback
 * is therefore a determinization choice, not native parity -- the same shape as
 * the nice normalization in sched_identity and the membarrier query-mask
 * normalization.
 *
 * All assertions are process-local and carry no host-derived value, so the
 * output (`ioprio ok=5`) is identical across runs, backends, and hosts.
 */

#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_ioprio_get
#define SYS_ioprio_get 252
#endif
#ifndef SYS_ioprio_set
#define SYS_ioprio_set 251
#endif

#define IOPRIO_WHO_PROCESS 1
#define IOPRIO_CLASS_SHIFT 13
#define IOPRIO_CLASS_BE 2
#define IOPRIO_PRIO_VALUE(cls, data) (((cls) << IOPRIO_CLASS_SHIFT) | (data))

static long ioprio_get_self(void)
{
	return syscall(SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0);
}

static long ioprio_set_self(int value)
{
	return syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, value);
}

int main(void)
{
	int ok = 0;

	/* A fresh guest reports the neutral I/O priority (0). */
	errno = 0;
	if (ioprio_get_self() == 0 && errno == 0)
		ok += 1;

	/* Requesting a best-effort priority is accepted... */
	if (ioprio_set_self(IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, 4)) == 0)
		ok += 1;

	/* ...but Hermit determinizes the value, so it stays 0
	 * (native would report the value just set). */
	if (ioprio_get_self() == 0)
		ok += 1;

	/* A second, distinct request is likewise accepted... */
	if (ioprio_set_self(IOPRIO_PRIO_VALUE(IOPRIO_CLASS_BE, 0)) == 0)
		ok += 1;

	/* ...and the readback is still the determinized 0. */
	if (ioprio_get_self() == 0)
		ok += 1;

	printf("ioprio ok=%d\n", ok);
	return 0;
}

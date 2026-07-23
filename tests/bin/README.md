# POSIX timer signal-delivery probe

`posix_timer_test.c` arms a one-shot `CLOCK_MONOTONIC` POSIX timer for
10 ms with `SIGEV_SIGNAL` and waits up to 100 ms for `SIGALRM`.

Build the guest from the repository root:

```sh
cc -std=c11 -O2 -Wall -Wextra -Werror \
  tests/bin/posix_timer_test.c -o posix_timer_test -lrt
```

Native Linux delivers the signal and exits 0:

```text
PASS: SIGALRM delivered after POSIX timer expiration
```

The current ptrace backend does not synthesize the configured signal when the
emulated timer expires. With default logging and no relaxations, this command:

```sh
target/release/hermit run --strict -- ./posix_timer_test
```

exits 1 after advancing past the bounded virtual-time deadline:

```text
FAIL: SIGALRM was not delivered within 100 ms of virtual time
```

The expected failure should become a success assertion when deterministic
`SIGEV_SIGNAL` delivery is implemented.

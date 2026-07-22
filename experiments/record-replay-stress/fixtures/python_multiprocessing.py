#!/usr/bin/env python3

import multiprocessing

WORKERS = 4
ITERATIONS = 10_000
MASK = (1 << 64) - 1


def mix(worker: int) -> int:
    state = worker + 1
    for iteration in range(1, ITERATIONS + 1):
        state = ((state ^ iteration) * 1_103_515_245 + 12_345) & MASK
    return state


def run_worker(worker: int) -> None:
    print(f"worker={worker} checksum={mix(worker)}", flush=True)


def main() -> None:
    context = multiprocessing.get_context("fork")
    processes = [
        context.Process(target=run_worker, args=(worker,))
        for worker in range(WORKERS)
    ]

    for process in processes:
        process.start()
    for process in processes:
        process.join()
        if process.exitcode != 0:
            raise SystemExit(f"worker {process.pid} exited with {process.exitcode}")

    checksum = 0
    for worker in range(WORKERS):
        checksum ^= mix(worker)
    print(f"joined={','.join(str(process.exitcode) for process in processes)} checksum={checksum}")


if __name__ == "__main__":
    main()

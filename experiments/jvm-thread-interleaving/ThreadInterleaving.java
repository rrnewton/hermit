import java.util.concurrent.CountDownLatch;

public final class ThreadInterleaving {
    private static final Object TRACE_LOCK = new Object();
    private static final StringBuilder TRACE = new StringBuilder();
    private static int traceEvents;

    private ThreadInterleaving() {}

    private static int positiveInt(String value, String name) {
        int parsed = Integer.parseInt(value);
        if (parsed <= 0) {
            throw new IllegalArgumentException(name + " must be positive");
        }
        return parsed;
    }

    public static void main(String[] args) throws Exception {
        int threadCount = args.length > 0 ? positiveInt(args[0], "threads") : 12;
        int rounds = args.length > 1 ? positiveInt(args[1], "rounds") : 48;

        CountDownLatch ready = new CountDownLatch(threadCount);
        CountDownLatch start = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(threadCount);
        long[] results = new long[threadCount];

        for (int threadId = 0; threadId < threadCount; threadId++) {
            final int id = threadId;
            Thread worker = new Thread(() -> {
                try {
                    ready.countDown();
                    start.await();

                    long local = 0x9e3779b97f4a7c15L ^ id;
                    for (int round = 0; round < rounds; round++) {
                        int work = 64 + ((id * 17 + round * 31) & 127);
                        for (int spin = 0; spin < work; spin++) {
                            local = Long.rotateLeft(local ^ (spin + round), 7)
                                    * 0x2545f4914f6cdd1dL;
                        }

                        if (((id + round) & 1) == 0) {
                            Thread.yield();
                        }

                        synchronized (TRACE_LOCK) {
                            TRACE.append(id).append(':').append(round).append(',');
                            traceEvents++;
                        }

                        if (((id * 3 + round) & 3) == 0) {
                            Thread.yield();
                        }
                    }
                    results[id] = local;
                } catch (InterruptedException error) {
                    Thread.currentThread().interrupt();
                    throw new AssertionError("worker interrupted", error);
                } finally {
                    done.countDown();
                }
            }, "trace-worker-" + id);
            worker.start();
        }

        ready.await();
        start.countDown();
        done.await();

        int expectedEvents = threadCount * rounds;
        if (traceEvents != expectedEvents) {
            throw new AssertionError(
                    "trace lost events: expected " + expectedEvents + ", got " + traceEvents);
        }

        long resultCheck = 0;
        for (long result : results) {
            resultCheck ^= result;
        }
        if (resultCheck == 0) {
            throw new AssertionError("worker computation was unexpectedly empty");
        }

        System.out.println(
                "THREAD_TRACE threads=" + threadCount
                        + " rounds=" + rounds
                        + " events=" + traceEvents);
        System.out.println(TRACE.toString());
    }
}

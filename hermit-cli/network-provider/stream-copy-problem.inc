static __attribute__((noinline)) void stream_copy_problem(struct ap_fd_call *call,u64 problem) {
    call->original.problem|=problem;
    if(AP_NATIVE_COPY_VERSION==5) {
        const u64 pointer=call->selection.word&~3ULL;
        struct ap_fd_file *file=pointer?lookup(&fd_files,&pointer):0;
        ap_stream_frontier_poison(file);
    }
    /* A blocking original Read need not reach EXIT promptly after a failed
     * observation. Publish refusal now; the existing actual-task terminal
     * owner still has to settle its original Call and retained records. */
    fd_problem(problem);
}

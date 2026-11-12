import subprocess, concurrent.futures, glob, os
target = "/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101/temp/cargo-target-322bp"
bin_path = [p for p in glob.glob(target + "/debug/deps/cli-*") if not p.endswith(".d")][0]
tests = ["run_kvm_timerfd_virtual_abstime", "run_kvm_timerfd_virtual_nonblock",
         "run_kvm_timerfd_virtual_scm_rights_epoll_is_refused_by_name"]
def run(spec):
    t, r = spec
    p = subprocess.run([bin_path, t, "--exact"], capture_output=True, text=True)
    ok = "1 passed" in p.stdout + p.stderr
    return f"round={r} {t} {'PASS' if ok else 'FAIL'}"
specs = [(t, r) for r in range(1, 6) for t in tests]
with concurrent.futures.ThreadPoolExecutor(max_workers=3) as ex:
    for line in sorted(ex.map(run, specs)):
        print(line)
print("fixture cache:", glob.glob(target + "/tmp/timerfd-virtual-time-fixture-*"))
for d in glob.glob(target + "/tmp/timerfd-virtual-time-fixture-*"):
    print(d, sorted(os.listdir(d)))

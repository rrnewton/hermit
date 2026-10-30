#!/usr/bin/env python3
import subprocess, json, sys
def run(args, cwd=None):
    r = subprocess.run(args, capture_output=True, text=True, cwd=cwd)
    return r.stdout.strip(), r.returncode

H = "/home/newton/work/dev-hermit/worktrees/slots/musetrain-20261101"
R = "/home/newton/work/dev-hermit/reverie"
hermit = [4948,4938,4935,4934,4933,4926,4920,4909,4907,4902,4892,4889,4888,4886,4885,4884,4879,4878,4877,4873,4867,4863,4857,4847,4845,4835,4802,4801,4796,4795,4792,4791,4789,4780,4779,4777,4774,4772,4769,4766,4764,4761,4759,4754,4753,4752,4751,4750,4748,4747,4746,4745,4743,4742,4741,4739,4735,4732,4729,4728,4724,4716,4714,4706,4701,4685,4683,4671,4664,4654,4648,4641,4640,4634,4630,4627,4626,4614,4610,4609,4608,4593]
rev = [1172,1168,1167,1158,1157,1156,1153,1150,1149,1145,1141,1137]

def patchids(repo, base, head):
    ids = {}
    out, _ = run(["git","log","--format=%H", f"{base}..{head}"], cwd=repo)
    for c in out.splitlines():
        d, _ = run(["git","show",c], cwd=repo)
        p, rc = run(["git","patch-id","--stable"], cwd=repo) if False else (None,None)
        # pipe diff into patch-id
        r = subprocess.run(["git","patch-id","--stable"], input=d, capture_output=True, text=True, cwd=repo)
        parts = r.stdout.split()
        if parts: ids[c] = parts[0]
    return ids

def analyze(repo, base, refprefix, numbers, main_patchids=None):
    rows = {}
    allpids = {}
    for n in numbers:
        head = f"{refprefix}{n}"
        sha, _ = run(["git","rev-parse",head], cwd=repo)
        mb, _ = run(["git","merge-base",base,head], cwd=repo)
        ahead, _ = run(["git","rev-list","--count",f"{base}..{head}"], cwd=repo)
        behind, _ = run(["git","rev-list","--count",f"{head}..{base}"], cwd=repo)
        cherry, _ = run(["git","cherry",base,head], cwd=repo)
        plus = sum(1 for l in cherry.splitlines() if l.startswith("+"))
        minus = sum(1 for l in cherry.splitlines() if l.startswith("-"))
        files, _ = run(["git","diff","--name-only",f"{base}...{head}"], cwd=repo)
        stat, _ = run(["git","diff","--shortstat",f"{base}...{head}"], cwd=repo)
        # conflict check via merge-tree
        mt = subprocess.run(["git","merge-tree","--write-tree","--name-only",base,head], capture_output=True, text=True, cwd=repo)
        conflict = mt.returncode != 0
        cfiles = []
        if conflict:
            lines = mt.stdout.splitlines()
            cfiles = lines[1:] if len(lines)>1 else []
        pids = patchids(repo, base, head)
        allpids[n] = set(pids.values())
        rows[n] = dict(sha=sha[:12], ahead=ahead, behind=behind, cherry_plus=plus, cherry_minus=minus,
                       files=files.splitlines() if files else [], stat=stat, conflict=conflict,
                       conflict_files=[c for c in cfiles if c][:8], pids=sorted(pids.values()))
    # overlap between PRs
    overlaps = []
    nums = list(allpids)
    for i,a in enumerate(nums):
        for b in nums[i+1:]:
            inter = allpids[a] & allpids[b]
            if inter: overlaps.append((a,b,len(inter)))
    return rows, overlaps

hrows, hover = analyze(H, "origin/main", "refs/pr/h/", hermit)
print(json.dumps({"hermit":hrows,"hermit_overlaps":hover}, indent=1))
with open("/home/newton/work/dev-hermit/worktrees/slots/musetrain-20261101/temp/hermit-proof.json","w") as f:
    json.dump({"rows":hrows,"overlaps":hover}, f, indent=1)
rrows, rover = analyze(R, "refs/remotes/kvm/main", "refs/pr/r/", rev)
with open("/home/newton/work/dev-hermit/worktrees/slots/musetrain-20261101/temp/reverie-proof.json","w") as f:
    json.dump({"rows":rrows,"overlaps":rover}, f, indent=1)
print("REV DONE", file=sys.stderr)

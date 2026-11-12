#!/usr/bin/env python3
import json, re, subprocess, sys, os
from datetime import datetime, timezone

SLOT = "/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101"
NOW = datetime.now(timezone.utc)
OLD_CUTOFF = datetime(2026, 11, 5, 23, 59, 59, tzinfo=timezone.utc)

def load(p):
    with open(p) as f: return json.load(f)

def owner_line(body):
    if not body: return "—"
    m = re.search(r"^Owner:\s*(.+)$", body, re.M)
    return m.group(1).strip()[:60] if m else "— (no Owner: line)"

def bodies(path):
    d = load(path)
    out = {}
    for n in d["data"]["repository"]["pullRequests"]["nodes"]:
        out[n["number"]] = n.get("body") or ""
    return out

def queue_map(path):
    q = {}
    try: lines = open(path).read().splitlines()
    except FileNotFoundError: return q
    cur = None
    for ln in lines:
        m = re.match(r"#(\d+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(\S+)\s+(.*)", ln)
        if m:
            cur = int(m.group(1))
            q[cur] = {"status": m.group(2), "head": m.group(4), "title": m.group(6)[:50], "note": ""}
        elif cur and ln.strip().startswith("held:"):
            q[cur]["note"] = ln.strip()
    return q

def reviews(num, repo_word):
    hits = []
    rdir = "/home/newton/work/dev-hermit/ignored/reviews"
    for fn in sorted(os.listdir(rdir)):
        if re.search(rf"(?:^|[-_/]){num}(?:[-_\.]|$)", fn):
            p = os.path.join(rdir, fn)
            try: txt = open(p, errors="ignore").read()
            except Exception: continue
            for m in re.finditer(r"(APPROVED-AT|CHANGES_REQUESTED-AT|APPROVE)[^\n]{0,90}", txt):
                line = m.group(0).strip()
                sha = re.search(r"\b[0-9a-f]{40}\b", line)
                hits.append(f"{fn}: {line[:80]}")
    return hits[-2:] if hits else []

def head_date(repo_dir, num):
    r = subprocess.run(["git", "-C", repo_dir, "log", "-1", "--format=%cI", f"refs/remotes/pr/{num}"],
                       capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else ""

def process(repo, listf, bodyf, queuef, repo_dir):
    prs = load(listf); bmap = bodies(bodyf); qmap = queue_map(queuef)
    rows = []
    for pr in prs:
        num = pr["number"]
        created = datetime.fromisoformat(pr["createdAt"].replace("Z", "+00:00"))
        age_h = (NOW - created).total_seconds() / 3600
        old = created <= OLD_CUTOFF
        upd = head_date(repo_dir, num) or pr["updatedAt"]
        q = qmap.get(num)
        qs = "none"
        if q:
            qs = q["status"] + (f" ({q['note']})" if q["note"] else "") + f" head {q['head']}"
        rev = reviews(num, repo)
        revs = " | ".join(rev) if rev else "no verdict in ignored/reviews"
        labels = ",".join(l["name"] for l in pr.get("labels", []))
        rows.append({
            "num": num, "title": pr["title"].replace("|", "/")[:70],
            "url": f"https://github.com/rrnewton/{repo}/pull/{num}",
            "age": age_h, "old": old, "owner": owner_line(bmap.get(num, "")),
            "push": upd, "queue": qs, "review": revs,
            "head": pr["headRefOid"][:12], "draft": pr["isDraft"],
            "author": pr["author"]["login"], "labels": labels,
            "created": pr["createdAt"][:10],
        })
    rows.sort(key=lambda r: r["created"])
    return rows

def action(r):
    q = r["queue"]
    if "landed" in q: return ("close-landed", "queue records it landed; close with the landed SHA")
    if q.startswith("queued"): return ("land", "in the merge queue; let it run")
    if q.startswith("held") or "held" in q: return ("land (held)", "queued but held; dependency/hold must clear first")
    if "ejected" in q: return ("rebase", "queue ejected it; owner rebases and resubmits")
    if "CHANGES_REQUESTED" in r["review"]: return ("review-fix", "latest verdict requests changes at an older head")
    if "APPROVED-AT" in r["review"]: return ("land", "approved verdict on file; submit to the queue")
    return ("review", "no verdict at current head in ignored/reviews")

for repo, lf, bf, qf, rd, out in [
    ("hermit", f"{SLOT}/temp/triage-hermit.json", f"{SLOT}/temp/triage-hermit-bodies.json",
     f"{SLOT}/temp/triage-queuelist.txt", SLOT, "/tmp/triage-hermit-rows.json"),
    ("reverie", f"{SLOT}/temp/triage-reverie.json", f"{SLOT}/temp/triage-reverie-bodies.json",
     f"{SLOT}/temp/triage-queuelist-reverie.txt", f"{SLOT}/temp/reverie-triage", "/tmp/triage-reverie-rows.json"),
]:
    rows = process(repo, lf, bf, qf, rd)
    for r in rows:
        r["action"], r["reason"] = action(r)
    json.dump(rows, open(out, "w"), indent=1)
    print(repo, len(rows), "old(<=Nov5):", sum(1 for r in rows if r["old"]))

**DISPOSITION: REPRODUCIBLE — retained artifact is disposable, but has not been deleted.**

# Compatibility website artifact reproduction handoff

Task: `the-compatibility-artifact-generator-exists-only-in-an-uncommitted-tree`

The retained artifact has not been deleted. An independent audit passed the
remote-only reproduction described below, so the artifact bytes are now
reconstructible from public, pinned inputs.

## Retained artifact

The durable copy is:

`/home/newton/work/dev-hermit/worktrees/slots/scorecard-website/ignored/compatibility-website-artifacts/real-b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`

- Identity: `b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`
- Tree SHA-256: `596408567ce5b7f8ad4a564688bdbb80f9144abf2075082b0f0fbcc335b2e7d5`
- Inventory: 8,006 regular files and seven directories.
- Regular-file bytes: 245,867,217.
- Every regular file is mode `0444`; every directory is mode `0555`.

The original deterministic pair also remains in `/tmp`:

- `/tmp/compatibility-website-artifact-v3.fcujko/output/b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`
- `/tmp/compatibility-website-artifact-v3.fcujko/output-second/b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`

Those two outputs and the durable copy were previously proven byte-for-byte
and mode-for-mode identical. The `/tmp` copies are not durable evidence.

## Generator source checkpoint

The source checkpoint is now published in `rrnewton/dev-hermit`:

- Branch: `scorecard-artifact-generator-checkpoint`
- Commit: `2db92e6f11bf3559542a37adc429f62f93b5e92c`
- Parent: `64a467a51da30209aeff4e1988d4d6054c4c4fa2`
- Local source checkout:
  `/home/newton/temp/dev-hermit/scorecard-website-artifact-v3`

The checkpoint contains exactly the twelve `ci-hub` paths recorded by b49's
`reader_worktree_dirty_paths`; it does not include the nested Hermit checkout
change. An independent audit verified:

- the local branch and live GitHub branch both resolve to the exact checkpoint;
- all 19 generator-file hashes in b49 `build.json` match the checkpoint tree;
- all 33 reader-file hashes match the checkpoint tree;
- there are zero missing or mismatched recorded source files;
- the Hermit gitlink is unchanged at
  `e85aaf9654983116ac26ae02beb8f95f7c46f02f` in both the parent and checkpoint;
- the commit contains only text source, tests, and documentation, with no
  artifact, binary, cache, experiment, `ai_docs`, nested repository, or symlink
  entry.

## Why checking out the checkpoint normally cannot reproduce b49 literally

b49 truthfully embeds the source state that generated it:

- `reader_commit=64a467a51da30209aeff4e1988d4d6054c4c4fa2`;
- `reader_worktree_clean=false`;
- the exact twelve dirty paths;
- the dirty source identity; and
- generator-base Hermit gitlink
  `e85aaf9654983116ac26ae02beb8f95f7c46f02f`.

A normal clean checkout with `2db92e6f...` as `HEAD` necessarily records a
different commit, clean state, dirty-path set, source identity, and artifact
identity. That is truthful behavior, not nondeterminism. Literal b49
reproduction must start at committed base `64a467a...` and overlay exactly the
twelve path blobs from committed checkpoint `2db92e6f...`. The resulting
working tree is intentionally dirty in exactly the way recorded by b49, but
every byte used to construct it is now committed and remotely reachable.

## Recorded data and manifest inputs

- dev-hermit ledger commit:
  `5bfa5d91f56e380631a549bf8dcf58631c97dabd`
- Record: `devbig014-1788153725-3415639`
- Validation run:
  `validate-mega-lander-69b08cd09a60-1788152123591813608-3265480-d6eafa4c`
- Measured Hermit commit:
  `69b08cd09a60705e8c04a93720439d9367b1f7a8`
- Manifest Hermit checkout:
  `b605c4a1892ab0cef1dc88587c11d2904d27d752`
- Manifest checkout's `agent-utils` gitlink:
  `bf09769daafcfecf83f80dc2cb762099c0be532e`
- Hermit main commit used by the ancestry projection:
  `98f1c079cf6368a433fcf2d122367d2179d18699`

All eight ledger and series source blobs recorded by b49 still match their
recorded SHA-256 values at the named dev-hermit ledger commit.

## Newly discovered Hermit object dependency

The historical projection asks the supplied Hermit object database whether
each of 85 recorded series tree commits is present and whether it is an
ancestor of `98f1c079...`. Presence of unrelated local objects therefore
changes the result even when all named commits and files are identical.

A fresh GitHub Hermit checkout omitted ten off-main commits. The first clean
attempt used source:

`/tmp/scorecard-b49-clean-rebuild.MRZXkecD/dev-hermit`

and produced the now-swept output:

`/dev/shm/compatibility-generator-repro.xMuYSvz3/output/fb076480a1333cf70dc3fe5a25c64556df814893c51a974a799f57501fb4c859`

It had the same path and mode sets as b49, but 495 file hashes differed.

A second attempt used an isolated copy of the original Hermit object store at:

`/tmp/scorecard-b49-ancestry-repro.IzgS34Lg/hermit`

and produced the now-swept output:

`/dev/shm/scorecard-b49-ancestry-build.Y2shOTxx/output/7fc061ef71e4df624a989cf3e9c5a424cc7d77c6edd4790c7edfbb4a04745a9c`

That object store contained the ten missing commits plus additional ambient
objects. It had the same paths and modes as b49, but 837 file hashes differed.
The required input is therefore the exact object closure, neither a normal
clone nor the accumulated original object store.

## Successful minimal reconstruction

The successful isolated work area is:

- dev-hermit source: `/tmp/scorecard-minimal-closure.hMHCvQ63/dev-hermit`
- Hermit sender: `/tmp/scorecard-minimal-closure.hMHCvQ63/sender`
- fresh Hermit receiver: `/tmp/scorecard-minimal-closure.hMHCvQ63/receiver`
- generated output, now swept:
  `/dev/shm/scorecard-minimal-guard.TJD0tyDL/output/b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`

The sender contains synthetic commit
`8fc4e4bb350662c30db6889a8f4f841664b3da29`, with the empty tree and exactly
these ten ordered parents:

1. `0b2772a74fc70a1eb4b24ef3385cc1a51f9cc028`
2. `0b2d6e8c96f37e1e2a28a706e0e97c5891d08109`
3. `1ccb9b41c4a48bc378b06aab04fc3bfb1cf6078e`
4. `4733542cf689ac99061e83121f3e35d83ca881b8`
5. `4944fb5b3cc029459056a3b9743f0d0df3ad0209`
6. `623e48a01705c31e5a7aabf81762df3897c9a969`
7. `a81236cb866ed35126f37a762ec6cc0c316a3dc1`
8. `b1c18fdb1b9e90d4532402e5fd0583f5f9dc7026`
9. `dcdf94ac6bd7a6daa36c6f32d72852a4e7214882`
10. `ee35d662e598abb11a9dfda23bb4a69f8059abb7`

The synthetic commit uses author and committer
`Compatibility Website Reproduction <compatibility-reproduction@invalid>`,
timestamp `2000-01-01T00:00:00Z`, and message
`Retain exact Hermit ancestry inputs for b49 reproduction`.

A fresh non-shallow receiver fetched only Hermit `b605c4a...` and then only
the synthetic ref. Before the second fetch, all ten commits were absent. After
it, the complete 85-entry ancestry map was exactly:

- true: 39;
- false: 11;
- null: 35;
- canonical SHA-256:
  `2a40a239e85db94087543afb87e50bd40ac33f63f77fb9e20e8586a5b6bedfa9`.

That map is byte-for-byte identical to the retained b49 map. The historical
build took 767 seconds and produced:

- identity
  `b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`;
- tree SHA-256
  `596408567ce5b7f8ad4a564688bdbb80f9144abf2075082b0f0fbcc335b2e7d5`;
- 8,006 regular files and seven directories;
- 245,867,217 regular-file bytes.

Recursive comparison proved every relative path, regular-file byte, file
type, file mode, and regular-file size identical. Directory path/type/mode also
matched; directory inode sizes differed across filesystems and are not
generated artifact-file sizes.

## Independent remote-only reproduction: PASS

The generator checkpoint and the exact Hermit ancestry input are now public:

- `rrnewton/dev-hermit` branch `scorecard-artifact-generator-checkpoint`:
  `2db92e6f11bf3559542a37adc429f62f93b5e92c`;
- `rrnewton/hermit` branch `compatibility-artifact-b49a-ancestry`:
  `8fc4e4bb350662c30db6889a8f4f841664b3da29`.

An independent read-only audit verified both live refs and repeated the build
on `devbig030` from fresh GitHub clones. The clones had no Git alternates,
replace refs, `GIT_OBJECT_DIRECTORY`, or `GIT_ALTERNATE_OBJECT_DIRECTORIES`;
no local input from `devbig014` entered the build. The complete pinned recipe
requires all of the following, not the generator checkpoint alone:

- generator checkpoint `2db92e6f...`, overlaid as the exact twelve paths on
  base `64a467a...`;
- ledger commit `5bfa5d91...`;
- Hermit commit `b605c4a...`;
- `agent-utils` commit `bf09769d...`; and
- the ten-parent Hermit ancestry-retention commit `8fc4e4bb...`, which makes
  the otherwise absent historical commit objects available to the projection.

The generator source alone was explicitly refuted as a sufficient recipe:
the normal-clone attempt differed in 495 files, and the ambient-object-store
attempt differed in 837 files. The pinned ancestry object input is therefore a
required build input rather than incidental cache state.

The remote-only preflight reproduced the exact 85-entry ancestry map (39 true,
11 false, 35 null) and canonical digest
`2a40a239e85db94087543afb87e50bd40ac33f63f77fb9e20e8586a5b6bedfa9`.
The build completed in 755 seconds and reproduced:

- identity
  `b49a20c23b7c6a718611573073c6aa76dc1b76b12154e0b41738857cf70cef3a`;
- tree SHA-256
  `596408567ce5b7f8ad4a564688bdbb80f9144abf2075082b0f0fbcc335b2e7d5`;
- 8,006 regular files and seven directories;
- 245,867,217 regular-file bytes; and
- regular-file mode `0444` and directory mode `0555` only.

The independently recomputed canonical per-path manifest is 8,013 lines with
SHA-256
`d9bb79ee02d8b59cd633b5831fcc75a85ad903a12431b734b22500ebda6cc6c2`.
It is byte-for-byte identical for the remote output, the returned remote
receipt, and the retained artifact at the path recorded above.

## Exact reconstruction procedure

1. Make a fresh dev-hermit checkout at
   `64a467a51da30209aeff4e1988d4d6054c4c4fa2`.
2. Overlay exactly the twelve changed paths from
   `2db92e6f11bf3559542a37adc429f62f93b5e92c`, without moving `HEAD` from
   `64a467a...`. Verify all 19 generator and all 33 reader hashes against the
   retained `build.json`.
3. Make a fresh, non-shallow Hermit checkout at
   `b605c4a1892ab0cef1dc88587c11d2904d27d752` and initialize `agent-utils` at
   `bf09769daafcfecf83f80dc2cb762099c0be532e`.
4. Fetch only public ref
   `refs/heads/compatibility-artifact-b49a-ancestry` at
   `8fc4e4bb350662c30db6889a8f4f841664b3da29` into that Hermit object
   database. Before building, require the exact 85-entry `39/11/35` ancestry
   map and digest `2a40a239...` above.
5. From the reconstructed dev-hermit tree run:

   ```bash
   ./ci-hub/ci-hub compatibility-website build-historical \
     --parent /absolute/path/to/reconstructed-dev-hermit \
     --parent-commit 5bfa5d91f56e380631a549bf8dcf58631c97dabd \
     --record-id devbig014-1788153725-3415639 \
     --run-id validate-mega-lander-69b08cd09a60-1788152123591813608-3265480-d6eafa4c \
     --manifest-hermit /absolute/path/to/reconstructed-dev-hermit/hermit \
     --manifest-commit b605c4a1892ab0cef1dc88587c11d2904d27d752 \
     --output-root /absolute/path/to/new-user-owned-empty-output-root
   ```

6. Require identity `b49a20c2...`, tree SHA-256 `59640856...`, and exhaustive
   equality of all paths, regular-file bytes, types, modes, and regular-file
   sizes against the retained artifact.

## Current durability state

Both required refs are public, and the independent reproduction from
remote-only inputs passed. The retained artifact is therefore disposable by
reproduction, but it remains present at the durable path recorded above and no
deletion was performed as part of this disposition update. The earlier local
work areas and failed-attempt history remain documented because they establish
why the public ten-parent ancestry input is load-bearing.

# Compatibility website publication

The `Docs` workflow rebuilds the compatibility website from the existing public
[`hermit_test_ledger`](https://github.com/rrnewton/hermit_test_ledger) every day at
08:23 UTC. GitHub schedules can be delayed. The workflow must be on `main`,
enabled, and have the source credential below before the nightly rebuild can
run successfully. A workflow file on a feature branch does not activate it.

The same workflow publishes the complete documentation site: rustdoc, the
Hermetic Infra landing page, every retained compatibility build, and
`compatibility/latest/`. It has no pull-request or main-push trigger. All
publication runs are serialized and restricted to `rrnewton/hermit:main`.

## Source access

The maintained website builder is in the private `rrnewton/dev-hermit`
repository. Hermit's `GITHUB_TOKEN` can publish Hermit release assets and Pages,
but cannot read that other private repository. A repository administrator must:

1. Create an SSH deploy key dedicated to this website workflow. Add its **public**
   key to `rrnewton/dev-hermit` under **Settings → Deploy keys**, with write access
   disabled.
2. Put the corresponding **private** key in `rrnewton/hermit` under **Settings →
   Secrets and variables → Actions**, named `COMPATIBILITY_SOURCE_DEPLOY_KEY`.
   Do not put either private key material or a personal access token in Git,
   workflow output, release assets, or review reports.
3. Confirm the exact commits in `.github/compatibility-site-builder.json` are
   available in their named repositories. The parent commit's recorded Hermit
   gitlink must equal `hermit_commit`. Update these pins together when a reviewed
   builder or presentation change is delivered.

The checkout action uses the read-only key only to fetch the pinned parent
source and does not persist credentials. Subsequent setup initializes only
public Hermit, its Agent Utils dependency, and the public cell ledger. It does
not recursively materialize unrelated parent or backend dependencies. Missing
source access fails the job before rebuilding; an old archive is never reported
as a fresh ledger rebuild.

## Run and inspect a fresh rebuild

Once the workflow is on `main` and source access is configured:

```bash
gh workflow run docs.yml --repo rrnewton/hermit --ref main \
  -f rebuild_compatibility=true
```

This fetches the cell ledger at run time, captures its full immutable commit,
and passes that exact commit to the maintained builder with published-only
selection. The recorded parent validation-history snapshot remains pinned to
the builder source; the workflow does not fetch a new private history or invent
missing result artifacts. The source pin describes the builder, not a claim
that all measurements ran at that source revision.

Fresh ledger rows do not bypass validation authority. A row produced by
`validate` needs the matching admitted terminal record and selected-cell scope
in that pinned parent snapshot. Later rows without that record remain explicit
exclusions until the parent snapshot advances. Pressure-test and repeat rows
retain their existing independent admission rules. A fetched ledger commit is
therefore not a claim that every newly published cell received comparison credit.

The build time, source commit dates, ledger snapshot commit, and measurement
timestamps are separate facts. Rebuilding does not run Hermit tests, create
new measurements, change a failed or missing result, or refresh measurement
timestamps. The existing typed reducer continues to determine the selected
cells, comparison credit, reference denominator and history.

The job validates the built website, creates an immutable checksum-bound
archive and append-only release registry, and requests a stable, non-draft data
release with `--latest=false`. It downloads and validates every retained archive
before deploying the full site. After deployment it compares nine ordinary
public URLs with the exact built manifest, page, CSS, JavaScript and browser
data. A stale or unreadable result makes the job fail. The uploaded receipt
records the exact source commits, archive identity and deployment result;
private source and credentials are not uploaded. Build success alone does not
establish public deployment.

## Publish a separately built website

A reviewed local build can reach Pages without the private-source credential.
Keep its new archive immutable, append its exact descriptor to a registry that
preserves every checked-in and already-served build, and upload the archive and
registry as stable release assets. Then dispatch the main workflow with the
registry asset ID and SHA-256:

```bash
gh workflow run docs.yml --repo rrnewton/hermit --ref main \
  -f compatibility_registry_asset=ASSET_ID \
  -f compatibility_registry_sha256=REGISTRY_SHA256
```

Leave `rebuild_compatibility` false for this handoff. Combining a fresh rebuild
with an explicit registry is refused. A manual Docs run with neither option
preserves the latest already-served registry, so rebuilding rustdoc cannot
silently roll the website back to an older checked-in snapshot.

Before this workflow change reaches `main`, the older manual Docs workflow has
no registry inputs. Its supported handoff is a reviewed registry-only source
change to `.github/compatibility-site-releases.json`, followed by the unchanged
manual main workflow. Never replace an old archive's bytes or reuse its content
identity for a new presentation.

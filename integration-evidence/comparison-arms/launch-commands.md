# Reviewed-but-not-launched command templates

Run neither command while another agent owns KVM validation capacity.

PR529-only:

```text
env PYTHONDONTWRITEBYTECODE=1 python3 /home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/comparison-arms/pr529-only/launch-pr529-only.py launch --unit validate-kvm-pr529-only-5c0bae-prepared-a1 --measurement /home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/brs-pr529-only-c632-3367082-1789015440618621238
```

This will create, and refuses to overwrite:

- `/home/newton/work/dev-hermit/ignored/validate/runs/validate-kvm-pr529-only-5c0bae-prepared-a1.json`
- `/home/newton/work/dev-hermit/ignored/validate/validate-kvm-pr529-only-5c0bae-prepared-a1.log`

Vectored-only:

```text
env PYTHONDONTWRITEBYTECODE=1 python3 /home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/comparison-arms/vectored-only/launch-vectored-only.py launch --unit validate-kvm-vectored-only-5c0bae-prepared-a1 --measurement /home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/brs-vectored-only-90ad-3372702-1789015450305808498
```

This will create, and refuses to overwrite:

- `/home/newton/work/dev-hermit/ignored/validate/runs/validate-kvm-vectored-only-5c0bae-prepared-a1.json`
- `/home/newton/work/dev-hermit/ignored/validate/validate-kvm-vectored-only-5c0bae-prepared-a1.log`

Each Python wrapper creates a current-schema `kind=bench,state=launching` record,
reserves the canonical log, starts a retained bounded `systemd-run --user
--collect` unit, changes the record to `running`, and invokes the exact shell
launcher through `ci-hub validate-lock run --kind bench --max 1 --wait 600
--hold 600 --child-deadline 5400`. It records completed/failed state and the
exact exit code at termination.

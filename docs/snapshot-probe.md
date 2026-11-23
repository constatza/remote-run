# Snapshot step 0: probe results

Phase D (snapshots) rests on three assumptions about a shared environment (a gate that had to pass before any snapshot code was written). This file records each probe with its date and versions. No snapshot code is written before all three pass on the real host.

## Workstation, 2026-11-23 (uv 0.13.9, Python 3.14.10, Linux)

Setup: a project `main/` with `src/pkg` (`WHERE = "main"`), a top-level `topmod.py`, a `uv_build` backend, and `uv sync` (an editable install: `.venv/.../*.pth` points at `main/src`). A snapshot directory `snap/` holds the same `pyproject.toml` and `uv.lock` without a `.venv`, with `src/pkg` saying `"snapshot"` and its own `topmod.py`. `UV_PROJECT_ENVIRONMENT=main/.venv` for every `uv` call.

| # | Check | Result |
|---|---|---|
| 1 | Import precedence: `PYTHONPATH=snap/src`, cwd `snap`, `import pkg, topmod` with the venv's python | pass: both come from `snap`. Without `PYTHONPATH` the editable install wins (`main/src`), so `PYTHONPATH` is what makes the snapshot's code run. Same result through `uv run --frozen --no-sync python -c`. |
| 3 | `uv run --frozen --no-sync` leaves the environment alone | pass: file list, sizes and mtimes of `.venv` are identical before and after. |
| 2 | `uv sync --frozen --check` in `snap` against `main/.venv`, same lock | **fail**: exit 1, "The environment is outdated". uv wants to reinstall the editable project `pkg` from the snapshot's path instead of `main`'s. It happens for any snapshot, so every run would refuse as a stale environment. The check never writes to the environment. |
| 2′ | `uv sync --frozen --check --inexact --no-install-project` in `snap` | pass: exit 0 for an identical lock; exit 1 when the snapshot's lock needs a package the environment lacks (`idna`); exit 0 for a change that touches only the project's own version (the project is not a dependency). The environment is not written. |

Consequence for the design: the stale check of a snapshot is `uv sync --frozen --check --inexact --no-install-project`, not the `uv sync --frozen --check` that `rr pull` runs in the checkout. `--inexact` means a package the environment has and the lock no longer lists is not reported; the check answers "does the environment hold what this lock needs", which is what a snapshot run depends on.

## Host (the test cluster), 2026-11-23 (uv 0.13.1, venv Python 3.15.0, CephFS home)

Run through `ssh the test cluster 'bash -l -s'` from a script, in one `mktemp -d` directory under `$HOME` that a trap removes (checked afterwards: nothing left). The system `python3` is 3.6.8, so `uv sync` used a uv-managed Python 3.15.0 and could fetch `uv_build`; the host has outbound access for uv. Same project layout as on the workstation, `UV_PROJECT_ENVIRONMENT` pointing at the main checkout's `.venv`.

| # | Check | Result |
|---|---|---|
| 1 | import precedence, `PYTHONPATH=snap/src`, cwd `snap` | pass: `pkg` and a top-level `topmod` both come from the snapshot (also through `uv run --frozen --no-sync`); without `PYTHONPATH` the editable install wins (`main`) |
| 3 | `uv run --frozen --no-sync` leaves the environment alone | pass: file list, sizes and mtimes unchanged |
| 2 | plain `uv sync --frozen --check` from the snapshot | fails as on the workstation: exit 1, environment unchanged |
| 2′ | `uv sync --frozen --check --inexact --no-install-project` | pass: exit 0 for the same lock, exit 1 for a lock that needs `idna`; environment unchanged |

All three assumptions hold on the host with the two-flag stale check, so Phase D is unblocked. `uv` is only on the PATH of a login shell there (`~/.local/bin`), so `rr`'s `env` file must put it on the PATH, as for `rr pull`.

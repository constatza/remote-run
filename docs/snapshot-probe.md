# Snapshot step 0: probe results

Phase D (snapshots) rests on three assumptions about a shared environment (`docs/plan.md`, Phase D, "Gate first"). This file records each probe with its date and versions. No snapshot code is written before all three pass on the real host.

## Workstation, 2026-11-23 (uv 0.13.9, Python 3.14.10, Linux)

Setup: a project `main/` with `src/pkg` (`WHERE = "main"`), a top-level `topmod.py`, a `uv_build` backend, and `uv sync` (an editable install: `.venv/.../*.pth` points at `main/src`). A snapshot directory `snap/` holds the same `pyproject.toml` and `uv.lock` without a `.venv`, with `src/pkg` saying `"snapshot"` and its own `topmod.py`. `UV_PROJECT_ENVIRONMENT=main/.venv` for every `uv` call.

| # | Check | Result |
|---|---|---|
| 1 | Import precedence: `PYTHONPATH=snap/src`, cwd `snap`, `import pkg, topmod` with the venv's python | pass: both come from `snap`. Without `PYTHONPATH` the editable install wins (`main/src`), so `PYTHONPATH` is what makes the snapshot's code run. Same result through `uv run --frozen --no-sync python -c`. |
| 3 | `uv run --frozen --no-sync` leaves the environment alone | pass: file list, sizes and mtimes of `.venv` are identical before and after. |
| 2 | `uv sync --frozen --check` in `snap` against `main/.venv`, same lock | **fail**: exit 1, "The environment is outdated". uv wants to reinstall the editable project `pkg` from the snapshot's path instead of `main`'s. It happens for any snapshot, so every run would refuse as a stale environment. The check never writes to the environment. |
| 2′ | `uv sync --frozen --check --inexact --no-install-project` in `snap` | pass: exit 0 for an identical lock; exit 1 when the snapshot's lock needs a package the environment lacks (`idna`); exit 0 for a change that touches only the project's own version (the project is not a dependency). The environment is not written. |

Consequence for the design: the stale check of a snapshot is `uv sync --frozen --check --inexact --no-install-project`, not the `uv sync --frozen --check` that `rr pull` runs in the checkout. `--inexact` means a package the environment has and the lock no longer lists is not reported; the check answers "does the environment hold what this lock needs", which is what a snapshot run depends on.

## Host (the test cluster)

Not run. It needs a throwaway project and a venv created on the cluster's filesystem, so it waits for the user's go-ahead (`AGENTS.md`: manual checks on the host stay read-only unless asked). To run: the same three checks with the host's `uv` and Python, in a directory under `$TMPDIR`, removed afterwards; record the `uv --version` and Python version here. `uv` is not on the PATH of a plain `bash -c` there (only `~/.local/bin` in a login shell), so use the same `--env` file that `rr` is configured with.

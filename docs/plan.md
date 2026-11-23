# Plan

Every phase of the old plan is built, reviewed and checked where it could be. This file keeps what is still open and the decisions that override `docs/feature-request.md`. Edit it in place; remove what is no longer true.

## Progress

The tree passes `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` (570 tests: 486 unit, 28 in the binary, 16 in `tests/snapshot.rs`, 17 doctests, 23 others).

| Area | State | Remaining |
|---|---|---|
| Safety base: `Host`, `--` before every host and path, trust store for `.rr.toml`, `sanitize`, job ids | done | stopping the `.rr.toml` search at `$HOME` is optional |
| Structure: `domain/` (pure), `ports.rs`, `adapters.rs`, `app/` (one file per command), `main.rs` prints and exits | done | none |
| `rr pull` guards | done; the building blocks were confirmed on the test cluster, `rr pull` itself was not run there | the window between the merge script's re-check and `git merge`; a job submitted as `sbatch proj/run.sh` from another directory is not seen |
| `rr wait`, `rr status`, `--json` | done; confirmed on the test cluster | `rr run --wait` with a checkout (it pulls, so it changes the checkout); `ssh -t` with a piped stdout; `--json` for the commands that write to stdout; a restart count in `status`; progress lines while waiting |
| `rr fwd` | done; confirmed on the test cluster against `127.0.0.1:22` | `xdg-open` behaviour for `--open`; SIGTERM and SIGHUP exit 130 like SIGINT; a launcher that exits non-zero after starting is not seen; two `rr fwd` of the same name at once are unsupported |
| Recipes (`rr do`), `rr x -s`, `rr acct -j` and `--format` | done; not run on the test cluster | ssh connection reuse (the README shows the `~/.ssh/config` lines; add a setting only when someone needs it per host), a tracked `rr.toml`, `rr-foo` subcommands |
| Snapshots (`rr run --mode snapshot`) and `rr gc` | done; run on the test cluster | see "Not built" and "Open limits" below |

## Decisions that override `docs/feature-request.md`

- **Exit codes.** `Exit` is `Success | Rr(RrCode) | Remote(NonZeroU8)`. `RrCode` is `Failure` (1), `Refused` (3), `Timeout` (4), `JobFailed` (5) or `Unknown` (6). A remote command's own code is always `Remote`, and `--json` says `"source": "rr"` or `"remote"`. Exit 3 is not split into several codes.
- **Snapshots are opt-in** (`--mode snapshot`; the default stays `checkout`), so the remote keeps running a commit that is on `origin` unless you ask otherwise.
- **Untracked files are opt-in** (`--include-untracked`), and names that look like secrets are refused unless named with `--allow-path`. Files staged as new count as untracked.
- **The snapshot id is the full 40-hex tree id.** Safety against name squatting comes from ownership and mode checks (root 700, snapshot 500), not from the length of the name.
- **The stale check of a snapshot** is `uv sync --frozen --check --inexact --no-install-project`. Plain `--check` always reports the editable project's own path as a difference (`docs/snapshot-probe.md`).
- **Upload.** The provenance goes into the script as a quoted word, not inside the tar. The snapshot is made read-only with `chmod -R a-w`, so its mode is 500. The upload script counts the extracted files against the cleared tree before it publishes, because GNU tar exits 0 on an archive cut between two members.
- **`rr gc`** takes `--older-than` (required, 1 h to 3650 d) and `--keep N`. It is a chain `Listed -> Trashed -> Rechecked -> remove` in which the deleting text exists only in `Rechecked::remove`. A refusal after an entry could not be put back is `GcOutcome::Stranded` (exit 1, JSON `result` `stranded`).
- **`rr gc` and the queue.** It reads the queue through the pull guard's own `job_loop` (`squeue` and `realpath -m`), and an unresolvable directory or a failing `squeue` refuses with nothing deleted. A job that `cd`s elsewhere is invisible; this is documented in the README.
- **Recipes** are argv lists, not shell strings. `pull = "checkout"` or `"none"`.
- **Pull guard.** The state script runs `git fetch -q --prune origin`; a target must be on an `origin/*` branch; the merge script re-asserts `HEAD == from`, a clean tree and the job guard, and exits 76 when any of them changed. The `.rr-snapshots` directory is excluded from the clean-tree check.
- **Dependencies.** `sha2` (trust digest), `serde_json` (`--json`), `tempfile` (scratch directories), and on Linux `libc` and on Unix `signal-hook` (`rr fwd` must end its ssh when `rr` is killed). The `url` crate was dropped: the URL is built from a `LoopbackPort`.

## Not built

- `rr do` with `pull = "snapshot"`.
- A cap on the number and total size of snapshots (`--snapshot-limit` is only the per-tree limit), `snapshot-keep` as a setting, and a `mode` setting in the config file.
- Opt-in ssh connection reuse.

## Open limits

- **`rr gc`:** the window between the second queue read and `rm`; two `rr gc` at once (no lock, because `flock` is unreliable on Lustre and NFS); a foreground process on the login node; the window between gc's read of `.rr-used` and a probe's touch of it; the other here-document loops in the gc script depend on bash writing a temporary file (a failure there keeps entries, never deletes one). `GcOutcome::Stranded` also fires when a first-pass rename failed and the queue then failed: exit 1 with nothing deleted, a message that is more alarming than the facts.
- **Local build:** a Ctrl-C or SIGKILL during the local snapshot build leaves the `rr-snapshot-*` temp directory (documented in the README). Cleaning it would need a signal handler around every local git call.
- **Terminal text:** a bare `\r` (a progress bar) in `rr log` shows as `?`, not as an updating line. `rr log -f` over `ssh -t` still needs a check on the real host.
- **`uv` on the test cluster** is only on the PATH of a login shell, so `rr pull` there needs `--env` or `RR_ENV` that sets it.

## Checked on the test cluster

2026-11-23, Slurm 26.05.4, uv 0.13.1, GNU coreutils 8.30, git 2.43, bash 4.4. Every check ran in a throwaway directory in the home that was removed afterwards.

- **Step 0** (`docs/snapshot-probe.md`): import precedence with `PYTHONPATH`, `uv run --frozen --no-sync` leaves the environment alone, and the two-flag stale check.
- **Pull building blocks:** `git branch -r --contains <sha> --list 'origin/*'` is non-empty for a pushed commit and exits 129 for an unknown one; `realpath -m` places `proj-runs` outside and `proj/sub/../x/` inside a checkout; `squeue --me -h -o '%i %Z'` and `timeout 60 sacct` run.
- **`rr wait` and `rr status`:** COMPLETED exits 0, FAILED 5, an id `sacct` does not list 6; `sbatch` prints `Submitted batch job N`; `sacct -X -j` on an array parent lists `<id>_1`, `<id>_2`; an ssh that fails on every poll ends as 6 after five polls.
- **`rr fwd`:** the listener is on `127.0.0.1` only and owned by the ssh child, SIGKILL of `rr` ends ssh, `--ttl` closes with exit 0, `--socket` leaves no file after a normal close.
- **Snapshots, first upload:** an uncommitted edit shipped and an untracked `.env` did not; `RR_DIRTY=true`; a second run of the same tree sent no archive; a changed file made a second snapshot; modes were 700 (root), 500 (snapshot) and 400 (files).
- **`rr gc --older-than 1h`:** kept two fresh snapshots, and after their `.rr-used` was aged by hand removed exactly the aged one each time. `squeue -t` takes the state names on Slurm 26.05.4, and an unknown one makes `squeue` exit 1.
- **`rr run --mode snapshot --wait`:** job 530084 ran from the snapshot (the job imported `pkg` from the snapshot's `src`, saw `RR_SNAPSHOT_ID` and `RR_DIRTY=false`) and the wait exited 0 on COMPLETED. After `uv add idna` the next run was refused at the stale check (exit 3, uv's "Would install idna" shown) before the command ran. `git status --porcelain -- . ':(exclude).rr-snapshots'` does not list the snapshot directory.
- **Not run there:** `rr pull` itself, `rr run` in checkout mode and `rr do` (they change a real working checkout), `xdg-open`.

## Verification

- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` must pass before any commit.
- Tests use `FakeExec` and `FakeRemote`, real `bash` with stub programs on `PATH`, and a stub `ssh` with a real temporary git repository (`tests/snapshot.rs`, `tests/json.rs`, `tests/fwd.rs`). None touches the network, `~/.ssh` or the real `HOME`.
- Manual checks on the test cluster stay read-only unless the user approves a throwaway job; record what a check confirmed in "Checked on the test cluster" above.

# rr feature requests

Status: proposed, not implemented. Each item is generic: it names no project, cluster or job system beyond what `rr` already assumes (ssh, rsync, git, Slurm). Items are ranked by value over effort. Every item lists what it changes for existing users and how it is tested (the existing `Remote` trait and a fake `ssh` on `PATH`; no real network).

## 1. Snapshot mode: run the working tree without commit and push

### Problem

`rr run` requires the code to be a commit on origin. A user who only wants to try an uncommitted change (or a `--dry-run` of a job) must commit, push and fast-forward the remote checkout. Four guards enforce that (`pull.rs:126-149`): commit on origin, remote tree clean, no active job submitted from the checkout, fast-forward only. A new untracked file is silently not shipped (`main.rs:202-207` warns for tracked files only), so the remote command then fails with "no such file".

### Proposal

Make the default of `rr run` ship the current working tree as an immutable, content-addressed snapshot. The old behaviour stays as `--mode checkout`.

1. **Build locally, touching nothing of the user's.** With a temporary index (`GIT_INDEX_FILE=<tmp> git read-tree HEAD && git add -A`), then `git write-tree` and `git commit-tree -p HEAD`. This honours `.gitignore`. Pin the ephemeral commit as `refs/rr/<tree12>` so `git show` works later. The user's index, branch and stash are never changed.
2. **Ship once, by content.** Stream `git archive <ephemeral>` over ssh into `<snapdir>/.tmp-<id>/`, then atomic rename to `<snapdir>/<tree12>/` and make it read-only. If `<tree12>` already exists, nothing is transferred. `<snapdir>` is a new config key (`snapshots`, default `<dir>/.rr-snapshots`).
3. **Record provenance in the snapshot.** `.rr/provenance.json` holds the tree id, the base `HEAD`, a dirty flag (tree differs from `HEAD`), the ephemeral commit sha, the sha of `uv.lock` if present and the rr version. `.rr/diff.patch` holds `git diff HEAD <ephemeral>`. Export `RR_SNAPSHOT_ID`, `RR_BASE_COMMIT`, `RR_DIRTY` and `RR_LOCK_SHA` to the remote command so the job can log them.
4. **Do not own the environment.** The snapshot never contains a virtual environment. Export `UV_PROJECT_ENVIRONMENT=<dir>/.venv` (the one shared, human-synced environment) and `PYTHONPATH=<snapshot>/src` (it precedes `.pth` entries), then run the command with the snapshot as the working directory. The stale-environment check becomes `uv sync --frozen --check` run in the snapshot, so it compares the snapshot's lock with the shared environment. Exit code 75 still maps to the refusal code.
5. **Guards.**
   - Stay: stale environment, ssh `BatchMode` and quoting, a snapshot size limit like the rsync `--limit`.
   - New: `rr gc [--older-than D]` deletes snapshots only if no active job uses them (`squeue -o %Z` equals the snapshot path). It never deletes a snapshot a requeue could still re-read.
   - Gone in snapshot mode: commit on origin, clean remote, jobs-from-checkout, fast-forward.
6. **Options.** `--mode checkout|snapshot`, `--require-clean` (refuse when the dirty flag is set; for results that must be reproducible from a commit), `--snapshot-limit`.

### Step 0: prove the environment assumptions before writing any snapshot code

The design rests on three assumptions about how the shared environment behaves. If the shared environment installed the project as an editable package, its `.pth` points at the main checkout, so imports could silently come from old library code while the job code comes from the snapshot. Run these as a throwaway probe, first on the workstation, then once on the host with its real environment and Python, and record the results with dates and versions in the repository's notes:

1. **Import precedence.** With an editable install of a small package in an environment, run `python -c "import pkg; print(pkg.__file__)"` from a snapshot directory with `PYTHONPATH=<snapshot>/src`. It must print the snapshot path. Also check a second import root outside `src/` (a top-level package imported from the working directory).
2. **Stale check from another directory.** With `UV_PROJECT_ENVIRONMENT=<main>/.venv`, run `uv sync --frozen --check` in the snapshot directory. It must exit 0 when the snapshot's lock equals the environment's, exit non-zero when the lock differs, and must not report the editable install's path as a difference. Also confirm it never writes to the environment.
3. **No hidden sync.** Run the job command with `uv run --frozen --no-sync` from the snapshot and confirm the environment's modification time and installed versions are unchanged.

Exit: all three behave as stated on the host. If 1 or 2 fails, the choices are a per-snapshot environment (slow and large on a shared filesystem) or keeping `--mode checkout` as the only mode for runs that need the shared environment. Decide before building, and update this request.

### Later phases

- Transport B: push the ephemeral commit to a ref in a repository on the host (`git push host:path <sha>:refs/rr/<id>`), then `git worktree add --detach`. Gives a real `.git`, so tools that read the git commit tag keep working.
- An environment keyed by the lock hash, built once on request and never mutated, so a lock change cannot alter packages under queued snapshot jobs.

### Breaking changes

The default of `rr run` changes. Justification: the commit-and-push step added no safety that an immutable, content-addressed snapshot with recorded provenance does not already give, and it blocked every dry run. `--mode checkout` keeps the old guarded behaviour for anyone who wants it. Scripts that depend on `rr run` fast-forwarding the remote checkout must pass `--mode checkout`.

### Tests

Pure: ephemeral-tree id is stable for equal content and differs when an untracked file is added; `decide` for snapshot mode lists only the guards above. Fake `Remote`: second run of the same tree transfers nothing; `gc` refuses a snapshot whose path appears in the fake `squeue` output; `--require-clean` refuses a dirty tree.

## 2. `rr wait`, machine-readable output, distinct exit codes

### Problem

Callers (people and scripts) poll `rr q` or `rr acct` until a job ends; `rr log -f` uses `tail -F` and never ends by itself. All four refusal reasons share exit code 3, so a caller can only tell them apart by reading stderr.

### Proposal

- `rr wait <job>... [--timeout S] [--poll S]`: polls `sacct -X -n -P -j` until every job is terminal. Exit 0 if all are COMPLETED, 1 otherwise, 4 on timeout. Prints one line per job (state, elapsed, exit code).
- `rr run --wait`: parse Slurm's `Submitted batch job N` from the command's stdout (generic, no knowledge of the submitting program), then behave like `rr wait N`.
- `rr status <job>`: one ssh call returning state, elapsed, restart count, working directory and the last N log lines.
- `--json` on `q`, `acct`, `status`, `pull` and `wait`. Use `sacct -P` and `squeue -o` with a delimiter; no Slurm JSON support needed.
- Split exit code 3 into distinct codes with a `reason` field in the JSON output: not-on-origin, remote-dirty, jobs-from-checkout, stale-environment (names to be fixed in the implementation). Keep 3 as the group for "refused" if cheap (for example, codes 3 and 10-13).

### Breaking changes

Callers that treat "exit 3" as the only refusal must also accept the new codes. Document it in the README.

### Tests

Fake `sacct` output sequences (PENDING, RUNNING, COMPLETED / FAILED / TIMEOUT); a timeout case; JSON round trip for each command.

## 3. `rr fwd`: managed port forwards on demand

### Problem

Reaching a web service on the host (a dashboard, a tracking server UI) means hand-typing `ssh -L` or keeping a long-lived alias, and nothing stops a forgotten tunnel. Other tools in the same workflow duplicate small tunnel helpers.

### Proposal

```toml
# ~/.config/rr/config.toml
[forward.mlflow]
remote = "127.0.0.1:5050"            # as seen from the host
local  = 5051                        # optional: fixed so a browser can keep credentials; free port if busy
open   = "http://127.0.0.1:{port}/"  # optional URL for --open
```

- `rr fwd <name> [--open] [--idle 15m]`, `rr fwd <host:port> [--local P]`, `rr fwd --list`.
- Runs `ssh -N -o ExitOnForwardFailure=yes -o ControlMaster=no -o ControlPath=none -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -L 127.0.0.1:<port>:<remote> <host>` in the foreground. `ControlMaster=no` and `ControlPath=none` stop an existing ControlPersist master from adopting the forward and outliving the command.
- Waits until the local port accepts a connection, prints the URL, runs `xdg-open` (or the platform equivalent) with `--open`, then blocks until Ctrl-C, ssh exit or `--idle`.
- Binds `127.0.0.1` only. Never a wildcard address.
- No pid file: the foreground process is the state, so nothing stale remains. An optional idle timeout can wait for a later version; shortcut: no idle timeout in the first version, add it when a forgotten tunnel causes trouble.

### Size and tests

About 150 lines: a `forward` table in `config.rs` (the parser refuses unknown keys today, so old binaries reject new files), a pure `fwd_argv` function, a small runner, no new crates. Tests: the argv as data; a fake `ssh` on `PATH` that listens on the port, and a variant that exits 1 immediately.

## 4. Recipes, scripts over stdin, smaller items

- **Recipes.** `[recipes.<name>]` with `run = "<command with {placeholders}>"`, optional `pull = "snapshot|checkout|none"`, `wait = true`, `json = true`. Invoked as `rr do <name> [-- args]`. Lives in a tracked `rr.toml` next to the project, while `.rr.toml` stays personal (host and directory). A project can then name its common remote commands once, and callers stop re-deriving them. Adds config schema (old binaries reject the new file).
- **`rr x -s <script>`**: send a local script over stdin to a configured interpreter (`rr x -s probe.py --interp "python -"`), running outside the project directory. `stream()` already inherits stdin, so `rr x -- python - < f.py` probably works today; this removes the quoting and sets the working directory.
- **ssh connection reuse.** Default `ControlMaster=auto`, `ControlPersist=10m`, a `ControlPath` under the user's runtime directory, on non-Windows. A `rr run` opens about 3 connections today. `rr fwd` opts out (item 3).
- **External subcommands** like cargo (`rr-foo` on `PATH` receives the resolved settings as environment variables). About 20 lines; defer until a second consumer exists.
- **`rr acct -j <ids>`** filter and a `--format` choice.
- **README drift.** The tagline promises "submit, watch" but no `submit` or `watch` command exists; either implement item 2 or reword.

## Suggested order

1 (after step 0 of item 1), 2, 3, then 4. Items 2 and 3 are independent of 1 and can ship first.

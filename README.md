# remote-run (`rr`)

Work on a remote Slurm cluster from your own machine.

With `rr` you can:

- run commands and named recipes in a remote project;
- look at the queue, job history and logs;
- wait for jobs to end;
- fast-forward the remote checkout, or run code that is not pushed yet;
- forward a port and copy small files.

Everything goes through your system `ssh`. Your `~/.ssh/config` (aliases, `User`, `IdentityFile`, `ProxyJump`), ssh-agent and `known_hosts` apply as they do for `ssh <host>`.

`rr` adds three checks that a plain alias leaves out:

- The remote checkout runs exactly the commit you have locally, and its tree is clean.
- Updating the checkout never changes the code under a job that is queued or running from it. Slurm restarts a requeued job from the code in that directory.
- If `uv.lock` changed, you find out before a job runs with an out-of-date environment.

## Contents

1. [Install](#install)
2. [Set it up](#set-it-up)
3. [Everyday use](#everyday-use)
4. [Run your code](#run-your-code)
5. [Wait for jobs](#wait-for-jobs)
6. [Copy files](#copy-files)
7. [Recipes and scripts](#recipes-and-scripts)
8. [Run code that is not pushed: snapshots](#run-code-that-is-not-pushed-snapshots)
9. [Clean up snapshots: `rr gc`](#clean-up-snapshots-rr-gc)
10. [Forward a port: `rr fwd`](#forward-a-port-rr-fwd)
11. [Reference](#reference): exit codes, `--json`, terminal output, `rr pull` guards
12. [Notes](#notes) and [Development](#development)

## Install

You need a Rust toolchain.

- From a clone: `cargo install --path .`
- From GitHub: `cargo install --git https://github.com/constatza/remote-run`
- Upgrade: the same command plus `--force`.

`cargo install` puts `rr` in `~/.cargo/bin`. That directory must be on your `PATH`. Rustup adds it; a system-packaged Rust does not.

## Set it up

You need two things: the cluster, and the project directory on it.

1. Put the cluster in your user file, `~/.config/rr/config.toml`:

   ```toml
   host = "mycluster"
   env = "setup/env.sh"
   ```

2. Put the project directory in `.rr.toml` at the project root:

   ```toml
   dir = "projects/myproject"
   ```

3. Review and trust the project file once:

   ```sh
   rr trust
   rr trust --yes
   ```

4. Check it works:

   ```sh
   rr q
   ```

### The settings

| Setting | Flag | Variable | Meaning |
|---|---|---|---|
| `host` | `--host` | `RR_HOST` | ssh host or alias |
| `dir` | `--dir` | `RR_DIR` | remote project directory, relative to the remote home or absolute; may not start with `-` |
| `env` | `--env` | `RR_ENV` | remote shell file sourced before every command (for example the one that puts `uv` on `PATH`); relative to the remote home |

- A setting comes from the first source that sets it: the flag, the variable, the nearest `.rr.toml` (found by walking up from the current directory), the user file.
- The user file is `$XDG_CONFIG_HOME/rr/config.toml`, else `~/.config/rr/config.toml`, else `%APPDATA%\rr\config.toml`.
- Unknown keys are refused.
- `rr` sees the remote side as a non-interactive ssh shell. That shell usually does not read your rc files. This is why `env` exists.
- `[forward.<name>]` and `[recipe.<name>]` tables come from the two files only. They merge by name, and a name in `.rr.toml` replaces the same name in the user file.

### Why `rr trust`

A `.rr.toml` can choose the host, the remote directory, a shell file the remote sources and the commands of recipes. So `rr` refuses one you have not reviewed.

- `rr trust` shows the file's path, SHA-256 and content.
- `rr trust --yes` records that SHA-256 as reviewed.
- Editing the file makes it untrusted again.
- The file is personal. Keep it out of git.

### Faster repeated calls (optional)

Each `rr` call opens one or two ssh connections. With connection sharing, later calls reuse the first one. This works on Linux, not on Windows. Add to `~/.ssh/config`:

```
Host mycluster
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 10m
```

## Everyday use

Look at your jobs:

| Command | What it does |
|---|---|
| `rr q` | your queued and running jobs (`squeue --me`) |
| `rr acct` | your recent jobs (`sacct`) |
| `rr status <job>` | state, elapsed time, exit code, directory and the end of the log |
| `rr log <job> [-f] [-n 100]` | a job's log; `-f` follows it, Ctrl-C stops |
| `rr cancel <job>...` | `scancel` |

- Every command takes `--host`, `--dir`, `--env` and `--json`. `rr <command> --help` lists the options of one command.
- Job ids look like `123`, `123_4` or `123.batch`. `rr log`, `rr cancel` and `rr acct -j` accept nothing else.
- `rr status` shows the last `-n` lines of the log (default 10). `rr log` shows `-n` lines (default 40). Both take a single job.

### `rr acct`

- `--since <time>` picks how far back to look. The default is `now-1days`. Any `sacct -S` time works, such as `now-3days` or `2026-11-20`.
- `-j 7,8_1` lists the named jobs instead, however old. It cannot be combined with `--since`.
- `--format brief` is the default: id, name, state, elapsed, exit code.
- `--format full` adds partition, start, end, nodes, CPUs and requested memory.

## Run your code

The usual flow: push, then run.

```sh
git push
rr run -- uv run --frozen --no-sync python -m mypackage.cluster submit train-small
rr q
rr log 123456 -f
```

A shell alias keeps it short:

```sh
alias rsub='rr run -- uv run --frozen --no-sync python -m mypackage.cluster submit'
```

| Command | What it does |
|---|---|
| `rr run [-t] [--force] -- <cmd>...` | pull, then run the command in the remote project |
| `rr pull [--force]` | only fast-forward the remote checkout to your local `HEAD` |
| `rr x [-t] -- <cmd>...` | run a command in the remote project, with no pull |

- `-t` allocates a terminal, for interactive programs.
- `rr x` and `rr run` return the remote command's own exit code.
- A refused pull runs nothing.
- Code reaches the cluster only as a pushed commit. For code that is not pushed, see [snapshots](#run-code-that-is-not-pushed-snapshots).

### What `rr pull` checks

1. It takes your local `HEAD`. Uncommitted local changes stay local, and `rr` warns about them. Outside a git repository it uses `origin`'s default branch.
2. On the remote it runs `git fetch origin` and checks three things. A refusal at any check exits with code 3:
   - **On origin:** the commit exists on `origin`. Otherwise: push it first.
   - **Clean checkout:** the remote checkout has no uncommitted changes. The `.rr-snapshots` directory is left out of this check. Any other untracked file counts.
   - **No jobs from the checkout:** none of your active jobs was submitted from the checkout.
3. It runs `git merge --ff-only <commit>`, so a diverged remote fails instead of merging.
4. If the directory holds `uv.lock`, it runs `uv sync --frozen --check`. This checks the environment without changing it.
   - A mismatch exits with code 3 and prints what `uv` said.
   - A remote without `uv` on `PATH` is an error (exit 1), not a mismatch.
   - `rr` never runs `uv sync` itself.

About the job check:

- It judges a job by its submit directory (`squeue %Z`, normalised on the remote with `realpath -m`). A directory that cannot be normalised counts as inside.
- A job submitted from another directory, as in `sbatch proj/run.sh`, is not detected.
- `--force` skips this check only.
- A job submitted from a snapshot (`<dir>/.rr-snapshots/<tree id>`) counts as inside the checkout. It holds back a pull until it ends. This is deliberately conservative.

Exit codes of `rr pull`: 0 ok, 3 refused, 1 an `rr` error. Any other code is the remote command's own.

## Wait for jobs

```sh
rr wait 123456 123457
rr run --wait -- sbatch job.sh
```

- `rr wait <job>... [--timeout S] [--poll S]` waits until the jobs end.
- `rr run --wait -- <cmd>...` runs the command, then waits for every job it submitted.

Exit codes of a wait:

| Exit | Meaning |
|---|---|
| 0 | every job `COMPLETED` |
| 4 | timeout, with jobs still active |
| 5 | a job ended in a state other than `COMPLETED` |
| 6 | `rr` could not tell how the jobs ended |

### How `rr wait` behaves

- It asks `sacct` every `--poll` seconds (default 30, at least 5).
- It stops after `--timeout` seconds (default 86400, 24 hours).
- It stops at the first job that ended in any state other than `COMPLETED`, because a job that depends on it may never start.
- `REQUEUED` and the other active states keep it waiting.
- A state `rr` does not know never ends the wait. The timeout stops it, as exit 6 naming the state. Exit 4 needs a job read as active.
- A timeout also names the requested jobs `sacct` never listed.

### How `rr run --wait` behaves

- It shows the command's output as it comes. Then it waits for every job that printed `Submitted batch job <id>`.
- If the command exits non-zero, that is its code, and nothing is waited for.
- If the command exits 0 without such a line, `rr` exits 6, because nothing says what to wait for.
- A line that starts with `Submitted batch job ` but has no valid job id also ends as 6, because the wait would otherwise skip that job.
- `--timeout` and `--poll` need `--wait`. Put them before the command: after it, a flag belongs to the command.

### When polling fails

- A failed ssh or an empty `sacct` answer is retried five times, then ends as exit 6, never as 5. Accounting can lag a new submission, and the job may be fine.
- A poll that hangs is cut off, so it counts as one failed poll. `wait` and `status` run ssh with `ConnectTimeout=10`, close a link that is silent for 60 s, and run `sacct` under `timeout 60`. `x`, `run` and `log -f` keep your own ssh configuration.
- A job that already shows a state other than `COMPLETED` ends the wait as 5, even while another job is not listed yet.
- A wait whose `--timeout` passes without a single job read as active ends as 6, not 4.

## Copy files

```sh
rr get logs/run1.txt .
rr put config.yaml
```

- `rr get <remote> [local] [--limit 1G]` copies from the remote. A relative remote path is inside the project.
- `rr put <local> [remote] [--limit 1G]` copies to the remote. The default is the project directory. It never deletes.
- Both measure first (`rsync --dry-run --stats`) and refuse anything above `--limit`, 1G by default. They are meant for logs, configs and figures, not datasets.
- A leading `~/` means the remote home. No other `~` is expanded, because rsync runs with `-s` and the remote shell never sees the path.
- A local path with a `:` before its first `/` is copied as a file name, not as a remote host.
- `rsync` must exist on both machines. Windows has no native `rsync`.

## Recipes and scripts

### Recipes: `rr do`

A recipe names a command you run often. It lives in the user file or in `.rr.toml` (which needs `rr trust`).

```toml
[recipe.train]
run = ["python", "-m", "train", "--config={config}"]
pull = "checkout"
```

Run it:

```sh
rr do train --set config=base -- --fast
```

This runs `python -m train --config=base --fast`.

- The command is a list of words, never a shell string. Each word is quoted on its own, so a value cannot add a word or run anything.
- `{slot}` is filled by `--set slot=value`. Write `{{` and `}}` for a literal brace.
- `pull = "checkout"` (the default) makes it behave like `rr run`: the guarded fast-forward first, and a refusal runs nothing.
- `pull = "none"` makes it behave like `rr x`.
- `-t` allocates a terminal.
- `--force` works as for `rr pull`. It is refused, not ignored, for a recipe that does not pull.
- A slot without a value, a value for a slot the recipe does not have, and a slot set twice are errors.
- Values are put in as given and are not expanded again.
- A value is quoted but not guarded. One that starts with `-` reaches the program as an option. A recipe that takes a path or a branch should put `"--"` before its slot.

### Scripts: `rr x -s`

Send a local file to a remote interpreter on its standard input. Nothing to copy first, and no quoting of the script:

```sh
rr x -s probe.py --interp "python -"
```

- The interpreter runs in your remote home, after your `env` file. It does not run in the project directory, and nothing is pulled.
- The output is raw, like `rr x`. The exit code is the interpreter's.
- Use either `-s FILE --interp ...` or `-- <cmd>...`, never both.
- There is no terminal, because stdin is the script. `-t` is refused. ssh runs with `-T`, so a `RequestTTY` in your ssh config cannot put the script through a tty.
- The script reaches the interpreter whole, even when your `env` file reads standard input.
- An empty file is refused before ssh starts. The file is read whole, whatever its size.

How `--interp` is read:

- It is split on blanks. `'...'` and `"..."` group a word and stand for exactly their text.
- A backslash, an unclosed quote or an empty program is refused.
- Every word is quoted again for the remote shell, so `$HOME` in it stays text.

## Run code that is not pushed: snapshots

`rr run` runs a pushed commit. `--mode snapshot` runs what is on your disk instead, with no commit and no push.

```sh
rr run --mode snapshot -- uv run --frozen --no-sync python -m mypackage.cluster submit train-small
```

- `--mode checkout` is the default and has not changed.
- Every snapshot flag is refused (exit 1) without `--mode snapshot`. So is `--force`, which only lifts the pull's job guard.
- Snapshot mode is opt-in. The checkout mode keeps its promise that the remote runs a commit that is on `origin`.
- A snapshot is code you have not reviewed or pushed, on a shared filesystem, in a directory only you can enter.

### What is shipped

- The tracked files, with your uncommitted changes. This is the working tree as `git add -u` on top of `HEAD` would make it. It uses the content of the working tree, not of your index.
- Untracked files are not shipped. `rr` says which it left out.
- Ignored files are never shipped.
- Files staged as new count as untracked, because `HEAD` does not have them.
- The tree is named by its id (40 hex digits): `<dir>/.rr-snapshots/<tree id>`. The same content is the same snapshot. A second run of an unchanged tree asks the remote one cheap question and transfers nothing.

### Flags

| Flag | Effect |
|---|---|
| `--include-untracked` | ship untracked files too (never ignored files) |
| `--allow-path <path>` | allow one secret-looking untracked file; repeatable; the path is relative to the repository root; needs `--include-untracked` |
| `--allow-submodules` | do not refuse submodules and git-lfs pointer files (they are not archived as content) |
| `--snapshot-limit <size>` | largest tree of file content; default 256M |
| `--require-clean` | refuse a working tree that differs from `HEAD` |

### What is refused

A refusal exits 3, prints `rr: refused: <why>` and sends nothing.

- **Secret-looking names**, even with `--include-untracked`, unless each is named with `--allow-path`: `.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.gpg`, `*.htpasswd`, `id_*`, `*credentials*`, `*.kdbx`, `.netrc`, `.npmrc`, `.pypirc`, `.ssh/`, `.kube/config`, `service-account*.json`, and the like.
- **Submodules and git-lfs pointer files**, unless `--allow-submodules`.
- **Symlinks** whose target is absolute or has a `..`.
- **A tree over `--snapshot-limit`.** This is checked before any byte is sent.
- **A dirty tree**, with `--require-clean`. "Dirty" includes files the snapshot leaves out (an untracked file, one staged as new, a rename), not only a changed tree. `RR_DIRTY=true` and the `dirty` field of the provenance mean the same.
- **A merge in progress** (unmerged paths), always, because the files hold conflict markers.
- **A SHA-256 repository.** It is not supported.
- **A file `git archive` would leave out** (`export-ignore`).

Under `--json` a refusal is `{"kind":"snapshot","result":"refused","reason":...}`. The reasons are `dirty_tree`, `too_large`, `submodules`, `lfs_pointers`, `unsafe_symlinks`, `secret_names`, `merge_in_progress`, `archive_mismatch` and `snapshot_not_private`.

### How the command runs

The command runs in the snapshot directory with these variables:

| Variable | Value |
|---|---|
| `PYTHONPATH` | `<snapshot>/src:$PYTHONPATH`, so the snapshot's code wins over the editable install in the shared environment |
| `UV_PROJECT_ENVIRONMENT` | `<dir>/.venv`, the project's one environment, shared by every snapshot |
| `RR_SNAPSHOT_ID` | the tree id |
| `RR_BASE_COMMIT` | the commit the tree is based on |
| `RR_DIRTY` | `true` or `false` |

- Run with `uv run --frozen --no-sync`, so uv never syncs the shared environment from a snapshot.
- If the snapshot has a `uv.lock`, `rr` first runs `uv sync --frozen --check --inexact --no-install-project` there, against the shared environment. It answers "does the environment hold what this lock needs" without writing to it. Plain `--check` always reports the project's own editable path as a difference.
- A failure of that check exits 3 like a stale pull. It says "the environment is out of date or uv failed" and prints what `uv` said. Any non-zero answer of `uv` is read that way, so read its message.
- `uv` missing from the remote `PATH` is an error.
- The environment itself is yours to update: run `uv sync` on the remote, in the checkout.
- `--wait` works as for the checkout. A job reads the snapshot in place, so keep it until the job ends.
- Jobs submitted from a snapshot are inside the checkout, so they hold back `rr pull`.
- Next to the code, the snapshot holds `.rr/provenance.json` and `.rr-used`:
  - `provenance.json` records the tree, the base commit, dirty or not, the git id of `uv.lock` and the `rr` version. It is written from a struct, at the first upload of that tree.
  - `.rr-used` is touched by every run, and by a probe that finds the snapshot already there. `rr gc` judges a snapshot's age by it.

### How the upload stays safe

On your machine:

- The tree is built in a temporary index (`GIT_INDEX_FILE`). Your index, branch, stash and refs are never written.
- Its objects go to a scratch object directory (`GIT_OBJECT_DIRECTORY`), with your repository's objects as an alternate to read from. Even a refused tree leaves nothing in your `.git/objects`.
- The archive (`git archive`) is held in memory whole, up to the limit.

On the remote:

1. The archive goes over ssh to `tar -x --no-same-owner --no-same-permissions` in a private staging directory: `mktemp -d .tmp-*` under a root made with `mkdir -m 0700`, with umask 077.
2. The script counts the extracted files against the tree. An archive that arrived incomplete is refused and nothing is published.
3. The snapshot is made read-only (`chmod -R a-w`, so mode 500) and published with `mv -T`, which cannot replace or nest.
4. If another upload won the race, it is used only after it passes the same check.

Details:

- The staging directory is removed on exit, HUP, INT and TERM.
- The setgid bit that a shared-group project directory would hand to them is cleared, so the modes stay exactly 700 and 500.
- A run killed with SIGKILL leaves a `.tmp-*` directory behind and never a half snapshot.
- Before any byte is sent, and again at publication, the root and an existing snapshot must be a real directory (no symlink) owned by you, with mode 700 and 500. A directory that someone else made, or that is writable, is refused (exit 3, `snapshot_not_private`), because a name on a shared filesystem can be squatted.
- It needs GNU `stat`, `mktemp`, `mv` and `tar` on the remote.

## Clean up snapshots: `rr gc`

Snapshots pile up on a shared filesystem, one per distinct tree. `rr gc` deletes the ones nobody uses.

```sh
rr gc --older-than 14d --keep 3
```

It runs one script in the remote project directory. It prints what it removed, what it kept and why. `--json` gives the same as one document, `"kind":"gc"`.

### Options

- `--older-than <age>` is required. There is no "delete everything" default.
  - The age looks like `90m`, `12h` or `30d`.
  - The minimum is `1h`. A shorter one is a usage error (exit 2). The maximum is `3650d`.
  - A snapshot counts as that old when the last run in it is older than the age. The time is that of its `.rr-used`, or the directory's own time when that file is missing.
- `--keep <N>` never deletes the N newest snapshots, whatever their age. The default is 0.

### What it touches

- Only direct children of `<dir>/.rr-snapshots` named with a full lowercase tree id (40 hex digits), that are real directories owned by you. It never follows a symlink.
- A symlink or a file with such a name is reported as `not yours` and left alone. Every other name is ignored.
- It also sweeps `.tmp-*` directories (an upload that was killed) and `.trash-*` directories (a clean-up that was killed) that have not been touched for an hour, whatever `--older-than` says.

### When it refuses

It exits 3 and deletes nothing when:

- the snapshot root is not a real directory of yours with mode 700;
- the root resolves (`realpath`) to `/`, to your `$HOME`, or to nothing;
- it cannot read the queue (`"reason":"queue_unreadable"`).

### Which snapshots are in use

1. It reads your active jobs with `squeue --me -h -t <the active states> -o '%i %Z'`.
   - It runs under `env -u SQUEUE_STATES -u SQUEUE_PARTITION -u SQUEUE_ACCOUNT -u SQUEUE_QOS -u SQUEUE_NAMES -u SLURM_CLUSTERS`, so an exported filter cannot hide a job.
   - The active states are pending, running, suspended, stopped, completing, configuring, requeued, resizing, signaling, stage-out and special-exit. A job that has completed does not count.
2. It resolves each job's working directory with `realpath -m`. This is the same query and normalisation as the pull guard.
3. It keeps a snapshot when a job's directory is the snapshot or inside it. A job submitted from `<snapshot>/src` counts, and `<snapshot>x` does not.
4. A snapshot set aside as `.trash-<id>` counts under both names.
5. It does not guess. If `squeue` or `realpath` fails, a line has no directory, or a directory cannot be resolved, it refuses.

### The order inside the script

1. List what is old enough and unused.
2. Rename each to `.trash-<name>`. A job that starts after that cannot enter it by its old name.
3. Read the queue again. Put back and keep anything a job is now in.
4. Make the rest writable (`chmod -R u+w`, the trees are read-only) and remove it with `rm -rf --one-file-system`.

Failures:

- Any step that fails keeps the snapshot.
- A removal that fails half way is reported as `failed`.
- An entry that could not be set aside or put back is reported as `kept (error)`. Its snapshot may now exist only as `.trash-<id>`.
- If the second queue read fails after entries were set aside and one could not be put back, the run ends as exit 1 (not the clean refusal, exit 3), because an entry may be missing under its own name. Nothing was deleted.
- A removal that fails half way and an entry in `kept (error)` both end with exit 1. The next `rr gc` sweeps the `.trash-*` directory.
- It does not use `flock`. File locks are unreliable on Lustre and NFS, so safety comes from the age floor, the rename and the second check.

### Limits no check can remove

- **Slurm jobs only.** A foreground process that `rr run --mode snapshot` started on the login node is invisible to `rr gc`. Use an `--older-than` longer than your longest login-node run.
- **Jobs that hide the path.** A job that `cd`s elsewhere, copies the tree, or reads it by a path that does not show in `squeue` is invisible. Keep `--older-than` generous (weeks, not hours) and `--keep` a few.
- **A run that has not submitted yet.** It is protected only by its `.rr-used`, which is why the floor is an hour.
- **A short window** between the second check and the removal. A job submitted from a snapshot in that window loses its code. Do not run `rr gc` while you are submitting jobs from snapshots.
- **Two at once.** Do not run two `rr gc` on the same project. They do not coordinate.
- **Remote tools.** It needs GNU `stat`, `realpath`, `mv -T`, `chmod -R` and `rm --one-file-system`, like the upload.

## Forward a port: `rr fwd`

Reach a service on a compute node (MLflow, TensorBoard, Jupyter) from your machine:

```sh
rr fwd gpu-01:5000 --open
rr fwd mlflow --ttl 2h
rr fwd gpu-01:5000 --socket
```

- The first opens `http://127.0.0.1:<free port>/` in your browser.
- The second uses a forward named in the config and closes it after 2 hours.
- The third makes a Unix socket in `$XDG_RUNTIME_DIR`, for curl and other clients.

A named forward looks like this, in the user file or a trusted `.rr.toml`:

```toml
[forward.mlflow]
target = "gpu-01:5000"
local = 5051
```

`local` is optional. Without it a free port is picked on every run.

### What it guarantees

- It listens on `127.0.0.1` only. There is no way to bind another address.
- It prints the URL only when the listener on the port is ssh's own.
  - On Linux, `rr` matches the listening socket in `/proc/net/tcp` to the sockets of the ssh process it started.
  - A process that took the port after `rr` chose it is never announced, whatever it answers. Nothing is printed, no browser is opened, ssh is stopped, and the exit code is not 0.
  - The same holds if the port is taken, or ssh ends right after binding.
- TCP forwards, and so `--open`, need Linux. On other systems `rr` cannot prove the listener is ssh's, so it refuses a TCP forward. Use `--socket` there.
- It runs with `ControlMaster=no`, so ending ssh really closes the tunnel.

### When it ends

- After `--ttl` (default 8h, at most 7d).
- When ssh ends.
- On Ctrl-C, SIGTERM or SIGHUP (exit 130).
- On Linux, when `rr` is killed with SIGKILL. The kernel signals ssh. Other systems have no such guarantee.

### Ports and sockets

- A fixed `--local` port is checked free before ssh starts.
- By default each run picks a random port. A browser shares the cookies of `127.0.0.1` across ports, so two forwards on fixed ports can see each other's login.
- `--socket` creates `$XDG_RUNTIME_DIR/rr-<name>.sock`. `<name>` is the config name, or the remote port for `host:port`.
- The socket is private only because `XDG_RUNTIME_DIR` is (a directory only you can enter, as systemd makes it). `rr` does not check that, so do not point it at a shared directory.
- A browser cannot use a socket, so `--socket` does not combine with `--open` or `--local`.
- An existing file at the socket path is an error and is never removed.
- `rr` removes its own socket file when the forward ends. It does not when `rr` itself is killed with SIGKILL: then remove it yourself.

### Limits

- SIGTERM and SIGHUP end the forward with exit 130, like Ctrl-C.
- `--open` only starts the browser. A launcher that exits non-zero afterwards is not noticed.
- **Loopback is not private.** On a shared workstation every local user can reach `127.0.0.1:<port>`. Any web page open in your browser can send requests to it (cross-site requests, DNS rebinding). Do not forward a service that has no login of its own unless you accept that. `--socket` protects against other local users, but not against your own browser or programs.

## Reference

### Exit codes

| Exit | Meaning |
|---|---|
| 0 | done (for a wait: every job `COMPLETED`) |
| 1 | an `rr` error |
| 2 | wrong usage (with `--json`, one error document on stdout) |
| 3 | refused by a guard (pull, a snapshot, a copy over its limit, an `rr gc` that could not tell what is in use) |
| 4 | `rr wait` timed out with jobs still active |
| 5 | a job ended in a state other than `COMPLETED` |
| 6 | `rr` could not tell how the jobs ended (see [When polling fails](#when-polling-fails)), or `run --wait` found no job id |
| 130 | `rr fwd` was interrupted and closed its forward; ssh ending by itself keeps its own code |

`rr x` and `rr run` return the remote command's own code, which can be any number. So 1 to 6 can also be a command's code. `--json` says which: `"source": "rr"` or `"source": "remote"`.

### `--json`

`--json` (before or after the command) makes stdout exactly one JSON object, with `source`, `exit` and a `kind`.

It works for `status`, `wait`, `pull`, `trust`, `fwd`, `gc` and `run --wait`. It is refused (exit 1, before anything starts) for `q`, `acct`, `log`, `x`, `do`, `cancel`, `get`, `put` and `run` without `--wait`. Those write to stdout themselves (a program's text, or `scancel` and rsync), which cannot share it with a document.

General rules:

- Under `run --wait --json` the command's own output is shown on stderr instead, so stdout stays one document.
- The lines `rr` writes to stderr while a command runs stay text.
- An `rr` error is one document, `{"source":"rr","exit":1,"kind":"error","message":...}`, and nothing is written to stderr.
- A command line `rr` cannot parse is the same document with exit 2. `--help` and `--version` print as usual.
- Text from the cluster in the JSON has terminal control characters replaced, as in text mode.

By command:

- **`pull`**: a refusal names a `reason`.
  - `not_on_origin`
  - `remote_dirty`
  - `jobs_from_checkout`
  - `state_changed`: a commit, an edit or a job landed since `rr` read the checkout.
  - `queue_unreadable`: the queue could not be re-read just before the merge.
- **snapshot refusal**: `"kind":"snapshot"`, see [What is refused](#what-is-refused).
- **`gc`**: `"kind":"gc"`.
  - `"result":"cleaned"` has the lists `removed`, `kept` and `failed`. Each `kept` entry has a `why`: `newest`, `young`, `in_use`, `not_yours` or `error`.
  - `"result":"refused"` has a `reason`: `root_not_safe` or `queue_unreadable`.
  - `"result":"stranded"` (exit 1) is a refusal after entries were set aside and could not all be put back. It carries the `reason` and the `kept` list.
- **`fwd`**: one document when the forward ends, `"kind":"forward"`. The URL line stays on stderr.
  - `"result":"expired"` or `"interrupted"`, with `"source":"rr"`.
  - When ssh ends by itself with a non-zero code: `"source":"remote"`, `"result":"ssh_ended"`, and ssh's code as `exit`. A lost forward is not mistaken for a remote command's exit.
  - When ssh ends by itself with code 0: an `rr` error document, because nothing asked ssh to stop.

### What reaches your terminal

- `rr q`, `rr acct` and `rr log` print text a job or the cluster wrote. `rr` reads it line by line and replaces terminal control characters with `?`. That covers escape sequences (title changes, clipboard writes), bidirectional overrides, invisible format characters and tag characters.
  - A carriage return that ends a line (`\r\n`) is kept. `rr log -f` runs `ssh -t`, and a raw-mode terminal needs it to return to column 0.
  - Any other carriage return becomes `?`, so a progress bar that rewrites its line is not drawn.
  - Invalid UTF-8 shows as U+FFFD.
  - A line longer than 64 KiB is printed in pieces.
- `rr x` and `rr run` are raw by design. You chose that program, and `-t` sessions need its exact bytes. For the unfiltered bytes of a file, use `rr x -- cat <file>`.

## Notes

- **Remote shell.** The remote login shell must quote like POSIX `sh`. bash, zsh and sh do; fish, csh and tcsh do not. Every command runs under `bash -c '<script>'`. ssh hands that line to the login shell, which must pass the quoted script to `bash` unchanged, and `bash` must be on the remote `PATH`. The script runs with `set -euo pipefail`.
- **`rr pull` tools.** It needs `git` and GNU `realpath -m` on the remote. It reads the checkout in one ssh call and merges in a second.
  - The second call checks again that `HEAD` and a clean tree are what the first call saw.
  - Unless you passed `--force`, it also checks that no job was submitted from the checkout since. This is the same `squeue` and `realpath -m` test. A failing `squeue` or `realpath` refuses.
  - A mismatch exits 3. Only the moments between that check and `git merge`, inside one script, are left open.
- **`rr acct` and `rr log`.**
  - `sacct` has no `--me`, so `rr acct` passes `-u "$USER"`.
  - `rr acct -j` with an id `sacct` does not know prints only the header.
  - `sacct` reports the `--output` pattern unexpanded (`%x-%j.out`). `rr log` expands `%%`, `%A`, `%a`, `%J`, `%j`, `%u` and `%x`, and refuses any other placeholder.
- **Snapshot build directory.** `rr run --mode snapshot` builds in a temporary directory under your temp dir (`rr-snapshot-*`: the index and the scratch objects).
  - It is removed when the run ends normally or fails with an error.
  - An interrupt (Ctrl-C, SIGTERM) or a SIGKILL during the few seconds of the local build can leave it behind. It is safe to delete.

## Development

The gate is the same three commands locally and in CI:

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Pre-commit hooks run them for you. Install [pre-commit](https://pre-commit.com) (or `prek`), then once per clone:

```sh
pre-commit install --install-hooks --hook-type pre-commit --hook-type pre-push
```

- On commit: whitespace and file checks, `cargo fmt --check` and `cargo clippy`.
- On push: `cargo test`.
- The tests need `git` and `python3` on `PATH`.

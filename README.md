# remote-run (`rr`)

Work on a remote Slurm cluster from your own machine: run commands and named recipes in a remote project, send a local script to a remote interpreter, look at the queue, job history and logs, wait for jobs to end, fast-forward the remote checkout, forward a port, and copy small files. Everything goes through your system `ssh`, so `~/.ssh/config` (aliases, `User`, `IdentityFile`, `ProxyJump`), ssh-agent and `known_hosts` apply as they do for `ssh <host>`.

`rr` adds three checks that a plain alias leaves out:

- The remote checkout runs exactly the commit you have locally, and its tree is clean.
- Updating the checkout never changes the code under a job that is queued or running from it. Slurm restarts a requeued job from the code in that directory.
- If `uv.lock` changed, you find out before a job runs with an out-of-date environment.

## Install

Requires a Rust toolchain:

```sh
cargo install --path .                       # from a clone
cargo install --git https://github.com/constatza/remote-run
```

`cargo install` puts `rr` in `~/.cargo/bin`, so that directory must be on `PATH` (rustup adds it; a system-packaged Rust does not). Upgrade with the same command plus `--force`.

## Configure

`rr` reads three settings and two kinds of named tables. A setting is taken from the first source that sets it, in this order: the flag, the environment variable, the nearest `.rr.toml` found by walking up from the current directory, the user file. The tables, `[forward.<name>]` (see `rr fwd`) and `[recipe.<name>]` (see Recipes), come from the two files only and merge by name; a name in `.rr.toml` replaces the same name in the user file.

| Setting | Flag | Variable | Meaning |
|---|---|---|---|
| `host` | `--host` | `RR_HOST` | ssh host or alias |
| `dir` | `--dir` | `RR_DIR` | remote project directory, relative to the remote home or absolute; may not start with `-` |
| `env` | `--env` | `RR_ENV` | remote shell file sourced before every command, for example the one that puts `uv` on `PATH`; relative to the remote home |

The user file is `$XDG_CONFIG_HOME/rr/config.toml`, else `~/.config/rr/config.toml`, else `%APPDATA%\rr\config.toml`. Put settings about you and the cluster there:

```toml
# ~/.config/rr/config.toml
host = "mycluster"
env = "setup/env.sh"
```

Put the remote directory of a project in `.rr.toml` at the project root. The file is personal, so keep it out of git. `rr` refuses a project file you have not reviewed, because it can choose the host, the remote directory, a shell file the remote sources and the commands of recipes: run `rr trust` to see what applies, then `rr trust --yes` to record it. Editing the file makes it untrusted again:

```toml
# <project>/.rr.toml
dir = "projects/myproject"
```

`rr trust` shows the file's path, SHA-256 and content; `rr trust --yes` records that SHA-256 as reviewed. Unknown keys are refused. `rr` sees the remote side as a non-interactive ssh shell, which usually does not read your interactive rc files. That is why `env` exists.

### Faster repeated calls (optional)

Each `rr` call opens one or two ssh connections. With connection sharing, later calls reuse the first one. Add this to `~/.ssh/config` on Linux or macOS (not supported on Windows):

```
Host mycluster
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 10m
```

## Use

```sh
rr q                          # your queued and running jobs (squeue --me)
rr acct [--since now-3days | -j <job>,...] [--format brief|full]   # your recent jobs, or the named ones (sacct)
rr log <job> [-f] [-n 100]    # a job's log; -f follows it, Ctrl-C stops
rr cancel <job>...            # scancel
rr status <job> [-n 10]       # state, elapsed time, exit code, directory and the end of the log
rr wait <job>... [--timeout S] [--poll S]   # wait until the jobs end
rr x [-t] -- <cmd>...         # run a command in the remote project (-t: allocate a terminal)
rr x -s <file> --interp "python -"   # send a local script on stdin to the interpreter, run in your remote home (no -t)
rr do <recipe> [--set slot=value]... [-t] [--force] [-- <more words>]   # run a recipe from the config
rr pull [--force]             # fast-forward the remote checkout to your local HEAD
rr run [-t] [--force] [--wait [--timeout S] [--poll S]] -- <cmd>...   # pull, then run; --wait then waits for the jobs it submitted
rr run --mode snapshot [--include-untracked] [--allow-path P]... [--allow-submodules] [--snapshot-limit 256M] [--require-clean] -- <cmd>...   # ship the working tree, run in that copy
rr gc --older-than <age> [--keep N]   # delete snapshots no job uses and no run has used for that long (at least 1h)
rr get <remote> [local] [--limit 1G]   # rsync from the remote; relative paths are inside the project
rr put <local> [remote] [--limit 1G]   # rsync to the remote (default: the project directory); never deletes
rr fwd <host:port|name> [--open] [--socket] [--local P] [--ttl 8h]   # ssh port forward to localhost
rr trust [--yes]              # review the project file (.rr.toml); --yes records it as trusted
```

Every command takes `--host`, `--dir` and `--env` (the settings above) and `--json` (see `--json` below). `rr <command> --help` lists the options of one command.

What reaches your terminal:

- `rr q`, `rr acct` and `rr log` print text a job or the cluster wrote, so `rr` reads it line by line and replaces terminal control characters (escape sequences such as title changes and clipboard writes, bidirectional overrides, invisible format characters, tag characters) with `?`. A carriage return that ends a line (`\r\n`) is kept, because `rr log -f` runs `ssh -t` and a raw-mode terminal needs it to return to column 0; any other one becomes `?`, so a progress bar that rewrites its line is not drawn. Invalid UTF-8 shows as U+FFFD, and a line longer than 64 KiB is printed in pieces.
- `rr x` and `rr run` are raw by design: you chose that program, and `-t` sessions need its exact bytes. For the unfiltered bytes of a file, use `rr x -- cat <file>`.

Remote paths are absolute or relative to the project directory. Only a leading `~/` is read, as the remote home; no other `~` is expanded (rsync is run with `-s`, so the remote shell never sees the path). A local path with a `:` before its first `/` is copied as a file name, not as a remote host. `rr log` and `rr cancel` accept only job ids such as `123`, `123_4` or `123.batch`.

`get` and `put` measure first (`rsync --dry-run --stats`) and refuse anything above `--limit`, 1G by default. They are meant for logs, configs and figures, not datasets.

### `rr acct`

`rr acct` lists your jobs from `sacct`. `--since <time>` (default `now-1days`; any `sacct -S` time such as `now-3days` or `2026-11-20`) picks how far back to look. `-j 7,8_1` lists the named jobs instead, however old; it cannot be combined with `--since`, and each id must be a job id such as `123`, `123_4` or `123.batch`. `--format brief` (the default: id, name, state, elapsed, exit code) or `--format full` (adds partition, start, end, nodes, CPUs and requested memory) picks the columns; only those two sets exist.

### Scripts: `rr x -s`

`rr x -s probe.py --interp "python -"` sends a local file to a remote interpreter on its standard input, so there is nothing to copy first and no quoting of the script:

- The interpreter runs in your remote home after your `env` file, not in the project directory, and nothing is pulled.
- `--interp` is split on blanks; `'...'` and `"..."` group a word and stand for exactly their text. A backslash, an unclosed quote or an empty program is refused. Every word is quoted again for the remote shell, so `$HOME` in it stays text.
- There is no terminal (`-t` is refused), because stdin is the script. ssh runs with `-T`, so a `RequestTTY` in your ssh config cannot put the script through a tty. The script reaches the interpreter whole even when your `env` file reads standard input.
- An empty file is refused before ssh starts; the file is read whole, whatever its size. The command is either `-s FILE --interp ...` or `-- <cmd>...`, never both.
- Output is raw, like `rr x`, and the exit code is the interpreter's.

### Recipes

A recipe names a command you run often. It lives in the user file or in `.rr.toml` (so it needs `rr trust` there). The command is a list of words, never a shell string: each word is quoted on its own, so a value cannot add a word or run anything. `{slot}` is filled by `--set slot=value`; write `{{` and `}}` for a brace.

```toml
[recipe.train]
run = ["python", "-m", "train", "--config={config}"]
pull = "checkout"   # or "none"; the default is "checkout"
```

`rr do train --set config=base -- --fast` runs `python -m train --config=base --fast`. With `pull = "checkout"` it is `rr run`: the guarded fast-forward first, and a refusal runs nothing; `"none"` is `rr x`. `-t` allocates a terminal; `--force` is the same as for `rr pull` and is refused (not ignored) for a recipe that does not pull. A slot without a value, a value for a slot the recipe does not have and a slot set twice are errors. Values are put in as given and are not expanded again. A value is quoted but not guarded, so one that starts with `-` reaches the program as an option; a recipe that takes a path or a branch should put `"--"` before its slot.

### `rr status`, `rr wait` and `rr run --wait`

`rr status <job>` prints the job's state, elapsed time, exit code, working directory and the last `-n` (default 10) lines of its log in one call; `rr log <job>` shows `-n` (default 40) lines, and `-f` follows. Both take a single job id.

`rr wait` asks `sacct` every `--poll` seconds (default 30, at least 5) until every job has ended or `--timeout` seconds (default 86400, 24 hours) have passed. It stops at the first job that ended in any state other than `COMPLETED`, because a job that depends on it may never start. `REQUEUED` and the other active states keep it waiting; a state `rr` does not know never ends the wait, so the timeout is what stops it, as 6 naming the state (4 needs a job read as active). A timeout also names the requested jobs `sacct` never listed.

`rr run --wait` runs the command with its output shown as it comes, then waits for every job that printed `Submitted batch job <id>`. If the command exits non-zero, that is its code and nothing is waited for. If it exits 0 without such a line, `rr` exits 6, because nothing says what to wait for. `--timeout` and `--poll` need `--wait` and are read before the command only: after it, a flag belongs to the command. A line that starts with `Submitted batch job ` but has no valid job id ends as 6 as well, because the wait would otherwise skip that job.

### Exit codes

| Exit | Meaning |
|---|---|
| 0 | done (for a wait: every job `COMPLETED`) |
| 1 | an `rr` error |
| 2 | wrong usage (with `--json`, one error document on stdout) |
| 3 | refused by a guard (pull, a snapshot, a copy over its limit, `rr gc` that could not tell what is in use) |
| 4 | `rr wait` timed out with jobs still active |
| 5 | a job ended in a state other than `COMPLETED` |
| 6 | `rr` could not tell how the jobs ended: ssh or `sacct` failed or did not list a job yet, five polls in a row, a job in a state `rr` does not know at the timeout, or `run --wait` found no job id |
| 130 | `rr fwd` was interrupted (Ctrl-C, SIGTERM or SIGHUP) and closed its forward; ssh ending by itself keeps its own code |

A failed ssh or an empty `sacct` answer (accounting can lag a new submission) is retried five times and then ends as 6, never as 5: the job may be fine. A poll that hangs is cut off, so it counts as one such failed poll: the polls of `wait` and `status` (not `x`, `run` or `log -f`, which keep your own ssh configuration) run ssh with `ConnectTimeout=10` and a link silent for 60 s closed, and `sacct` itself runs under `timeout 60`, so a stalled accounting daemon on a live link is one failed poll too. A job that already shows a state other than `COMPLETED` ends the wait as 5 even while another job is not listed yet; a wait whose `--timeout` passes without a single job read as active ends as 6, not 4. `rr x` and `rr run` return the remote command's own code, which can be any number, so 1 to 6 can also be a command's code. `--json` says which: `"source": "rr"` or `"source": "remote"`.

### `--json`

`--json` (before or after the command) makes stdout exactly one JSON object, with `source`, `exit` and a `kind`, for `status`, `wait`, `pull`, `trust`, `fwd`, `gc` and `run --wait` (a refused `--mode snapshot` is `"kind":"snapshot"`, see above; `rr gc` is `"kind":"gc"`, with `"result":"cleaned"` and the lists `removed`, `kept` (each with a `why`: `newest`, `young`, `in_use`, `not_yours` or `error`) and `failed`, or `"result":"refused"` with a `reason`). Under `run --wait --json` the command's own output is shown on stderr instead, so stdout stays one document. A refused pull names a `reason`: `not_on_origin`, `remote_dirty`, `jobs_from_checkout`, `state_changed` (a commit, an edit or a job landed since `rr` read the checkout) or `queue_unreadable` (the queue could not be re-read just before the merge). An `rr` error under `--json` is one document too, `{"source":"rr","exit":1,"kind":"error","message":...}`, and nothing is written to stderr; a command line `rr` cannot parse is the same document with exit 2 (`--help` and `--version` print as usual). `rr fwd --json` prints one document when the forward ends (`"kind":"forward"`, `"result":"expired"` or `"interrupted"`, `"source":"rr"`); the URL line stays on stderr. When ssh ends by itself, a non-zero code gives `"source":"remote"`, `"kind":"forward"`, `"result":"ssh_ended"` with ssh's code as `exit` (a lost forward is not mistaken for a remote command's exit); code 0 gives an `rr` error document (`"source":"rr"`, `"kind":"error"`), because nothing asked ssh to stop. The lines `rr` writes to stderr while a command runs stay text. `q`, `acct`, `log`, `x`, `do`, `cancel`, `get`, `put` and `run` without `--wait` write to stdout themselves (a program's text, or `scancel` and rsync), which cannot share it with a document, so `--json` is refused for them (exit 1) before anything starts. Text from the cluster in the JSON has terminal control characters replaced, as in text mode.

### `rr pull`

Code reaches the cluster only as a pushed commit. Push first (`git push`), then:

1. `rr` takes your local `HEAD`. Uncommitted local changes stay local, and `rr` warns about them. Outside a git repository it uses `origin`'s default branch.
2. On the remote it runs `git fetch origin` and checks three things. A refusal at any check exits with code 3:
   - the commit exists on `origin` (otherwise "push it first");
   - the remote checkout has no uncommitted changes (the `.rr-snapshots` directory that `rr run --mode snapshot` keeps inside the checkout is left out of that check; any other untracked file counts);
   - none of your active jobs was submitted from the checkout, judged by the job's submit directory (`squeue %Z`, normalised on the remote with `realpath -m`; a directory that cannot be normalised counts as inside). A job submitted from another directory with `sbatch proj/run.sh` is not detected. `--force` skips this check only. A job submitted from a snapshot (`<dir>/.rr-snapshots/<tree id>`) was submitted from inside the checkout, so it holds back a pull until it ends. This is deliberately conservative.
3. It runs `git merge --ff-only <commit>`, so a diverged remote fails instead of merging. If the directory holds `uv.lock`, it also runs `uv sync --frozen --check`, which checks the environment without changing it. A mismatch exits with code 3 and prints what `uv` said; a remote without `uv` on `PATH` is an error (1), not a mismatch. `rr` never runs `uv sync` itself.

Exit codes: 0 ok, 3 refused, any other code is the remote command's own or an `rr` error (1).

### `rr run --mode snapshot`

`rr run` runs your code from the remote checkout after a guarded `rr pull`: the commit must be pushed. `--mode snapshot` runs what is on your disk instead, with no commit and no push. `--mode checkout` is the default and nothing about it changed; every flag below is refused (exit 1) without `--mode snapshot`, and so is `--force`, which only lifts the pull's job guard.

```sh
rr run --mode snapshot -- uv run --frozen --no-sync python -m mypackage.cluster submit train-small
```

What is shipped: the tracked files with your uncommitted changes (the working tree, as `git add -u` on top of `HEAD` would make it), at the content of the working tree, not of your index. Untracked files are not shipped; `rr` says which it left out. `--include-untracked` ships them too (never ignored files), and still refuses names that look like secrets (`.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.gpg`, `*.htpasswd`, `id_*`, `*credentials*`, `*.kdbx`, `.netrc`, `.npmrc`, `.pypirc`, `.ssh/`, `.kube/config`, `service-account*.json`, and the like) unless each is named with `--allow-path <path relative to the repository root>`, which needs `--include-untracked` (it is refused alone). Files staged as new count as untracked, because `HEAD` does not have them. Submodules and git-lfs pointer files are not archived as content, so they are refused unless `--allow-submodules`; a symlink whose target is absolute or has a `..` is refused; a tree over `--snapshot-limit` (default 256M of file content) is refused before any byte is sent; `--require-clean` refuses a working tree that differs from `HEAD`, and so does `RR_DIRTY=true` and the `dirty` field of the provenance: that includes files the snapshot leaves out (an untracked file, one staged as new, a rename), not only a changed tree. A merge in progress (unmerged paths) is always refused, because the files hold conflict markers. A SHA-256 repository is not supported. `git archive` leaving a file out (`export-ignore`) is refused too. A refusal exits 3, prints `rr: refused: <why>` and sends nothing; under `--json` it is `{"kind":"snapshot","result":"refused","reason":...}` with `dirty_tree`, `too_large`, `submodules`, `lfs_pointers`, `unsafe_symlinks`, `secret_names`, `merge_in_progress`, `archive_mismatch` or `snapshot_not_private`.

How it works:

- The tree is built locally in a temporary index (`GIT_INDEX_FILE`), so your index, branch, stash and refs are never written. Its objects go to a scratch object directory (`GIT_OBJECT_DIRECTORY`, with your repository's objects as an alternate to read from), so even a tree that is refused (a secret, a file over the limit) leaves nothing in your `.git/objects`; the scratch directory is removed when the run has shipped or refused the tree. Its id (40 hex digits) names the snapshot: `<dir>/.rr-snapshots/<tree id>`. The same content is the same snapshot, so a second run of an unchanged tree asks the remote one cheap question and transfers nothing.
- The archive (`git archive`) is held in memory whole, up to the limit, and goes over ssh to `tar -x --no-same-owner --no-same-permissions` in a private staging directory (`mktemp -d .tmp-*` under a root made with `mkdir -m 0700`, umask 077, removed on exit, HUP, INT and TERM; the setgid bit that a shared-group project directory would hand to them is cleared, so the modes stay exactly 700 and 500). The snapshot is made read-only (`chmod -R a-w`, so mode 500) and published with `mv -T`, which cannot replace or nest. A run killed with SIGKILL leaves a `.tmp-*` directory behind and never a half snapshot. If another upload won the race, it is used only after it passes the same check.
- Before any byte is sent and again at publication, the root and an existing snapshot must be a real directory (no symlink) owned by you with mode 700 and 500: a directory that someone else made, or that is writable, is refused (exit 3, `snapshot_not_private`), because a name on a shared filesystem can be squatted. Needs GNU `stat`, `mktemp`, `mv` and `tar` on the remote.
- Next to the code, `.rr/provenance.json` (tree, base commit, dirty or not, the git id of `uv.lock`, `rr` version; written from a struct, from the first upload of that tree) and `.rr-used` (touched by every run; `rr gc` judges a snapshot's age by it). Snapshots are removed by `rr gc`, never by `rr run`: see below.
- The command runs in the snapshot directory with `PYTHONPATH=<snapshot>/src:$PYTHONPATH` (the snapshot's code wins over the editable install in the shared environment), `UV_PROJECT_ENVIRONMENT=<dir>/.venv` (the project's one environment, shared by every snapshot) and `RR_SNAPSHOT_ID`, `RR_BASE_COMMIT` and `RR_DIRTY` (`true` or `false`). Run with `uv run --frozen --no-sync`, so uv never syncs the shared environment from a snapshot.
- If the snapshot has a `uv.lock`, `rr` first runs `uv sync --frozen --check --inexact --no-install-project` there, against the shared environment: it answers "does the environment hold what this lock needs" without writing to it (plain `--check` always reports the project's own editable path as a difference). A failure of that check exits 3 like a stale pull, says "the environment is out of date or uv failed" and prints what `uv` said (any non-zero answer of `uv` is read that way, so read its message); `uv` missing from the remote `PATH` is an error. The environment itself is yours to update (`uv sync` on the remote, in the checkout).
- `--wait` works as for the checkout: the command's output is shown, then the jobs it submitted are waited for. A job reads the snapshot in place, so keep it until the job ends. A job that `cd`s elsewhere or copies the tree is invisible to `rr gc` (see its limits below). Jobs submitted from a snapshot are inside the checkout, so they hold back `rr pull` (see above).
- The snapshot is code you have not reviewed or pushed, on a shared filesystem, in a directory only you can enter. It is not the default and nothing makes it so: the checkout mode keeps its promise that the remote runs a commit that is on `origin`.

### `rr gc`

```sh
rr gc --older-than 14d [--keep 3]
```

Snapshots pile up on a shared file system (one per distinct tree), so `rr gc` deletes the ones nobody uses. It runs one script in the remote project directory and prints what it removed, what it kept and why (`--json` gives the same as one document, `"kind":"gc"`).

- `--older-than <age>` is required: there is no "delete everything" default. The age is `90m`, `12h` or `30d`, at least `1h` (a shorter one is a usage error, exit 2) and at most `3650d`. A snapshot counts as that old when the last run in it (the time of its `.rr-used`; the directory's own time when that file is missing) is older than the age.
- `--keep <N>` never deletes the N newest snapshots (by that time), whatever their age. Default 0.
- What it looks at: only direct children of `<dir>/.rr-snapshots` named with a full lowercase tree id (40 hex digits), that are real directories (a symlink is never followed) owned by you. A symlink or a file with such a name is reported as `not yours` and left alone; every other name is ignored. It also sweeps `.tmp-*` directories (an upload that was killed) and `.trash-*` directories (a clean-up that was killed) that have not been touched for an hour, whatever `--older-than` says.
- It refuses (exit 3, nothing deleted) when the snapshot root is not a real directory of yours with mode 700, or resolves (`realpath`) to `/`, to your `$HOME`, or to nothing.
- Which snapshots are in use: it reads your active jobs with `squeue --me -h -t <the active states> -o '%i %Z'` run under `env -u SQUEUE_STATES -u SQUEUE_PARTITION -u SQUEUE_ACCOUNT -u SQUEUE_QOS -u SQUEUE_NAMES -u SLURM_CLUSTERS`, so an exported filter cannot hide a job (the states are pending, running, suspended, stopped, completing, configuring, requeued, resizing, signaling, stage-out and special-exit; a job that has completed does not count), resolves each job's working directory with `realpath -m` (the same query and normalisation as the pull guard), and keeps a snapshot when a job's directory is the snapshot or inside it (so a job submitted from `<snapshot>/src` counts, and `<snapshot>x` does not). A snapshot set aside as `.trash-<id>` counts under both names. If `squeue` or `realpath` fails, a line has no directory, or a directory cannot be resolved, it does not guess: it refuses (exit 3, `"reason":"queue_unreadable"`) and deletes nothing.
- The order inside the script is: list what is old enough and unused, rename each to `.trash-<name>` (a job that starts after that cannot enter it by its old name), read the queue again, put back and keep anything a job is now in, and only then make the rest writable (`chmod -R u+w`, the trees are read-only) and `rm -rf --one-file-system` it. Any step that fails keeps the snapshot; a removal that fails half way is reported as `failed`, and an entry that could not be set aside or put back as `kept (error)`, both with exit 1 and the `.trash-*` directory is swept by the next `rr gc`.
- It does not use `flock`: file locks are unreliable on Lustre and NFS, so safety comes from the age floor, the rename and the second check.

Limits, which no check can remove:

- Only Slurm jobs count. A foreground process that `rr run --mode snapshot` started on the login node is invisible to `rr gc`, so use an `--older-than` longer than your longest login-node run.
- A job that `cd`s elsewhere, copies the tree, or reads it by a path that does not show in `squeue` is invisible. Keep `--older-than` generous (weeks, not hours), and `--keep` a few. A run that has started in a snapshot but not yet submitted anything is protected only by its `.rr-used`, which is why the floor is an hour.
- Between the second check and the removal there is a short window; a job submitted from a snapshot in that window loses its code. Do not run `rr gc` while you are submitting jobs from snapshots.
- Do not run two `rr gc` at once on the same project; they do not coordinate.
- It needs GNU `stat`, `realpath`, `mv -T`, `chmod -R` and `rm --one-file-system` on the remote, like the upload.

### `rr fwd`

Reach a service on a compute node (MLflow, TensorBoard, Jupyter) from your machine:

```sh
rr fwd gpu-01:5000 --open     # http://127.0.0.1:<free port>/ in your browser
rr fwd mlflow --ttl 2h        # a forward named in the config, closed after 2 hours
rr fwd gpu-01:5000 --socket   # a Unix socket in $XDG_RUNTIME_DIR for curl and other clients
```

```toml
# ~/.config/rr/config.toml (a trusted .rr.toml may also have it)
[forward.mlflow]
target = "gpu-01:5000"
local = 5051                  # optional; without it a free port is picked on every run
```

- TCP forwards, and so `--open`, need Linux: `rr` proves the listener on the port is ssh's through `/proc`, and on other systems it cannot, so it refuses them. There, use `--socket`.
- It listens on `127.0.0.1` only; there is no way to bind another address. The URL is printed only when the listener on the port is ssh's own: on Linux `rr` matches the listening socket in `/proc/net/tcp` to the sockets of the ssh process it started, so a process that took the port after `rr` chose it is never announced, whatever it answers (nothing is printed, no browser is opened, ssh is stopped and the exit code is not 0). The same holds if the port is taken, or ssh ends right after binding. Off Linux `rr` cannot prove that, so it refuses a TCP forward and asks for `--socket`.
- The forward ends after `--ttl` (default 8h, at most 7d), when ssh ends, on Ctrl-C, SIGTERM or SIGHUP, and, on Linux, when `rr` is killed with SIGKILL (the kernel signals ssh; other systems have no such guarantee). It runs with `ControlMaster=no`, so ending ssh really closes the tunnel.
- A fixed `--local` port is checked free before ssh starts. By default each run picks a random port, because a browser shares the cookies of `127.0.0.1` across ports, so two forwards you keep open on fixed ports can see each other's login.
- `--socket` creates `$XDG_RUNTIME_DIR/rr-<name>.sock`; `<name>` is the config name, or the remote port for `host:port`. It is private only because `XDG_RUNTIME_DIR` is (a directory only you can enter, as systemd makes it); `rr` does not check that, so do not point it at a shared directory. A browser cannot use a socket, so `--socket` does not combine with `--open` or `--local`. An existing file at that path is an error and is never removed; `rr` removes its own socket file when the forward ends (not when `rr` itself is killed with SIGKILL: then remove it yourself).
- Known limits. SIGTERM and SIGHUP end the forward with exit 130, like Ctrl-C. `--open` only starts the browser: a launcher that exits non-zero afterwards is not noticed.
- Loopback is not private. On a shared workstation every local user can reach `127.0.0.1:<port>`, and any web page open in your browser can send requests to it (cross-site requests, DNS rebinding). Do not forward a service that has no login of its own unless you accept that; `--socket` protects against other local users but not against your own browser or programs.

### Example: a uv project with a submit command

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

## Notes

- The remote login shell must quote like POSIX `sh` (bash, zsh and sh do; fish, csh and tcsh do not). Every command runs under `bash -c '<script>'`: ssh hands that line to the login shell, which must pass the quoted script to `bash` unchanged, and `bash` must be on the remote `PATH`. The script runs with `set -euo pipefail`.
- `rr pull` needs `git` and GNU `realpath -m` on the remote. It reads the checkout in one ssh call and merges in a second; the second call checks again that `HEAD` and a clean tree are what the first call saw and, unless you passed `--force`, that no job was submitted from the checkout since (the same `squeue` and `realpath -m` test; a failing `squeue` or `realpath` refuses), and refuses (exit 3) if not. Only the moments between that check and `git merge`, inside one script, are left open.
- `sacct` has no `--me`, so `rr acct` passes `-u "$USER"`. `rr acct -j` with an id `sacct` does not know prints only the header. `sacct` reports the `--output` pattern unexpanded (`%x-%j.out`). `rr log` expands `%%`, `%A`, `%a`, `%J`, `%j`, `%u` and `%x`, and refuses any other placeholder.
- `rr run --mode snapshot` builds in a temporary directory under your temp dir (`rr-snapshot-*`: the index and the scratch objects). It is removed when the run ends normally or fails with an error; an interrupt (Ctrl-C, SIGTERM) or a SIGKILL during the few seconds of the local build can leave it behind, and it is safe to delete.
- `rsync` must exist on both machines. Windows has no native `rsync`.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

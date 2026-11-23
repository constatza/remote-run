# remote-run (`rr`)

Work on a remote Slurm cluster from your own machine: run commands in a remote project, look at the queue and job logs, wait for jobs to end, fast-forward the remote checkout, and copy small files. Everything goes through your system `ssh`, so `~/.ssh/config` (aliases, `User`, `IdentityFile`, `ProxyJump`), ssh-agent and `known_hosts` apply as they do for `ssh <host>`.

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

`rr` reads three settings. Each one is taken from the first source that sets it, in this order: the flag, the environment variable, the nearest `.rr.toml` found by walking up from the current directory, the user file.

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

Put the remote directory of a project in `.rr.toml` at the project root. The file is personal, so keep it out of git. `rr` refuses a project file you have not reviewed, because it can choose the host, the remote directory and a shell file the remote sources: run `rr trust` to see what applies, then `rr trust --yes` to record it. Editing the file makes it untrusted again:

```toml
# <project>/.rr.toml
dir = "projects/myproject"
```

Unknown keys are refused. `rr` sees the remote side as a non-interactive ssh shell, which usually does not read your interactive rc files. That is why `env` exists.

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
rr acct [--since now-3days]   # your recent jobs (sacct)
rr log <job> [-f] [-n 100]    # a job's log; -f follows it, Ctrl-C stops
rr cancel <job>...            # scancel
rr status <job> [-n 10]       # state, elapsed time, exit code, directory and the end of the log
rr wait <job>... [--timeout S] [--poll S]   # wait until the jobs end
rr x [-t] -- <cmd>...         # run a command in the remote project (-t: allocate a terminal)
rr pull [--force]             # fast-forward the remote checkout to your local HEAD
rr run [-t] [--force] [--wait] -- <cmd>...   # pull, then run; --wait then waits for the jobs it submitted
rr get <remote> [local]       # rsync from the remote; relative paths are inside the project
rr put <local> [remote]       # rsync to the remote (default: the project directory); never deletes
rr fwd <host:port|name> [--open] [--socket] [--local P] [--ttl 8h]   # ssh port forward to localhost
```

What reaches your terminal:

- `rr q`, `rr acct` and `rr log` print text a job or the cluster wrote, so `rr` reads it line by line and replaces terminal control characters (escape sequences such as title changes and clipboard writes, bidirectional overrides, invisible format characters, tag characters) with `?`. A carriage return that ends a line (`\r\n`) is kept, because `rr log -f` runs `ssh -t` and a raw-mode terminal needs it to return to column 0; any other one becomes `?`, so a progress bar that rewrites its line is not drawn. Invalid UTF-8 shows as U+FFFD, and a line longer than 64 KiB is printed in pieces.
- `rr x` and `rr run` are raw by design: you chose that program, and `-t` sessions need its exact bytes. For the unfiltered bytes of a file, use `rr x -- cat <file>`.

Remote paths are absolute or relative to the project directory. Only a leading `~/` is read, as the remote home; no other `~` is expanded (rsync is run with `-s`, so the remote shell never sees the path). A local path with a `:` before its first `/` is copied as a file name, not as a remote host. `rr log` and `rr cancel` accept only job ids such as `123`, `123_4` or `123.batch`.

`get` and `put` measure first (`rsync --dry-run --stats`) and refuse anything above `--limit`, 1G by default. They are meant for logs, configs and figures, not datasets.

### `rr wait`, `rr run --wait` and exit codes

`rr wait` asks `sacct` every `--poll` seconds (default 30, at least 5) until every job has ended or `--timeout` seconds (default 86400, 24 hours) have passed. It stops at the first job that ended in any state other than `COMPLETED`, because a job that depends on it may never start. `REQUEUED` and the other active states keep it waiting; a state `rr` does not know never ends the wait, so the timeout is what stops it, as 6 naming the state (4 needs a job read as active). A timeout also names the requested jobs `sacct` never listed.

`rr run --wait` runs the command with its output shown as it comes, then waits for every job that printed `Submitted batch job <id>`. If the command exits non-zero, that is its code and nothing is waited for. If it exits 0 without such a line, `rr` exits 6, because nothing says what to wait for. `--timeout` and `--poll` need `--wait` and are read before the command only: after it, a flag belongs to the command. A line that starts with `Submitted batch job ` but has no valid job id ends as 6 as well, because the wait would otherwise skip that job.

| Exit | Meaning |
|---|---|
| 0 | done (for a wait: every job `COMPLETED`) |
| 1 | an `rr` error |
| 2 | wrong usage (with `--json`, one error document on stdout) |
| 3 | refused by a guard (pull, a copy over its limit) |
| 4 | `rr wait` timed out with jobs still active |
| 5 | a job ended in a state other than `COMPLETED` |
| 6 | `rr` could not tell how the jobs ended: ssh or `sacct` failed or did not list a job yet, five polls in a row, a job in a state `rr` does not know at the timeout, or `run --wait` found no job id |
| 130 | `rr fwd` was interrupted (Ctrl-C, SIGTERM or SIGHUP) and closed its forward; ssh ending by itself keeps its own code |

A failed ssh or an empty `sacct` answer (accounting can lag a new submission) is retried five times and then ends as 6, never as 5: the job may be fine. A poll that hangs is cut off, so it counts as one such failed poll: the polls of `wait` and `status` (not `x`, `run` or `log -f`, which keep your own ssh configuration) run ssh with `ConnectTimeout=10` and a link silent for 60 s closed, and `sacct` itself runs under `timeout 60`, so a stalled accounting daemon on a live link is one failed poll too. A job that already shows a state other than `COMPLETED` ends the wait as 5 even while another job is not listed yet; a wait whose `--timeout` passes without a single job read as active ends as 6, not 4. `rr x` and `rr run` return the remote command's own code, which can be any number, so 1 to 6 can also be a command's code. `--json` says which: `"source": "rr"` or `"source": "remote"`.

`--json` (before or after the command) makes stdout exactly one JSON object, with `source`, `exit` and a `kind`, for `status`, `wait`, `pull`, `trust`, `fwd` and `run --wait`. Under `run --wait --json` the command's own output is shown on stderr instead, so stdout stays one document. A refused pull names a `reason`: `not_on_origin`, `remote_dirty`, `jobs_from_checkout` or `state_changed`. An `rr` error under `--json` is one document too, `{"source":"rr","exit":1,"kind":"error","message":...}`, and nothing is written to stderr; a command line `rr` cannot parse is the same document with exit 2 (`--help` and `--version` print as usual). `rr fwd --json` prints one document when the forward ends (`"kind":"forward"`, `"result":"expired"` or `"interrupted"`, `"source":"rr"`); the URL line stays on stderr. ssh ending by itself is `"source":"remote"`. The lines `rr` writes to stderr while a command runs stay text. `q`, `acct`, `log`, `x`, `cancel`, `get`, `put` and `run` without `--wait` write to stdout themselves (a program's text, or `scancel` and rsync), which cannot share it with a document, so `--json` is refused for them (exit 1) before anything starts. Text from the cluster in the JSON has terminal control characters replaced, as in text mode.

### `rr pull`

Code reaches the cluster only as a pushed commit. Push first (`git push`), then:

1. `rr` takes your local `HEAD`. Uncommitted local changes stay local, and `rr` warns about them. Outside a git repository it uses `origin`'s default branch.
2. On the remote it runs `git fetch origin` and checks three things. A refusal at any check exits with code 3:
   - the commit exists on `origin` (otherwise "push it first");
   - the remote checkout has no uncommitted changes;
   - none of your active jobs was submitted from the checkout, judged by the job's submit directory (`squeue %Z`, normalised on the remote with `realpath -m`; a directory that cannot be normalised counts as inside). A job submitted from another directory with `sbatch proj/run.sh` is not detected. `--force` skips this check only.
3. It runs `git merge --ff-only <commit>`, so a diverged remote fails instead of merging. If the directory holds `uv.lock`, it also runs `uv sync --frozen --check`, which checks the environment without changing it. A mismatch exits with code 3 and prints what `uv` said; a remote without `uv` on `PATH` is an error (1), not a mismatch. `rr` never runs `uv sync` itself.

Exit codes: 0 ok, 3 refused, any other code is the remote command's own or an `rr` error (1).

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
- `rr pull` needs `git` and GNU `realpath -m` on the remote. It reads the checkout in one ssh call and merges in a second; the second call checks again that `HEAD` and a clean tree are what the first call saw, and refuses (exit 3) if not.
- `sacct` has no `--me`, so `rr acct` passes `-u "$USER"`. `sacct` reports the `--output` pattern unexpanded (`%x-%j.out`). `rr log` expands `%%`, `%A`, `%a`, `%J`, `%j`, `%u` and `%x`, and refuses any other placeholder.
- `rsync` must exist on both machines. Windows has no native `rsync`.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

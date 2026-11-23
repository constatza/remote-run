# remote-run (`rr`)

Work on a remote Slurm cluster from your own machine: run commands in a remote project, look at the queue and job logs, fast-forward the remote checkout, and copy small files. Everything goes through your system `ssh`, so `~/.ssh/config` (aliases, `User`, `IdentityFile`, `ProxyJump`), ssh-agent and `known_hosts` apply as they do for `ssh <host>`.

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
rr x [-t] -- <cmd>...         # run a command in the remote project (-t: allocate a terminal)
rr pull [--force]             # fast-forward the remote checkout to your local HEAD
rr run [-t] [--force] -- <cmd>...   # pull, then run
rr get <remote> [local]       # rsync from the remote; relative paths are inside the project
rr put <local> [remote]       # rsync to the remote (default: the project directory); never deletes
```

What reaches your terminal:

- `rr q`, `rr acct` and `rr log` print text a job or the cluster wrote, so `rr` reads it line by line and replaces terminal control characters (escape sequences such as title changes and clipboard writes, bidirectional overrides, invisible format characters) with `?`. A carriage return that ends a line is dropped; any other one becomes `?`. Invalid UTF-8 shows as U+FFFD, and a line longer than 64 KiB is printed in pieces.
- `rr x` and `rr run` are raw by design: you chose that program, and `-t` sessions need its exact bytes. For the unfiltered bytes of a file, use `rr x -- cat <file>`.

Remote paths are absolute or relative to the project directory. Only a leading `~/` is read, as the remote home; no other `~` is expanded (rsync is run with `-s`, so the remote shell never sees the path). A local path with a `:` before its first `/` is copied as a file name, not as a remote host. `rr log` and `rr cancel` accept only job ids such as `123`, `123_4` or `123.batch`.

`get` and `put` measure first (`rsync --dry-run --stats`) and refuse anything above `--limit`, 1G by default. They are meant for logs, configs and figures, not datasets.

### `rr pull`

Code reaches the cluster only as a pushed commit. Push first (`git push`), then:

1. `rr` takes your local `HEAD`. Uncommitted local changes stay local, and `rr` warns about them. Outside a git repository it uses `origin`'s default branch.
2. On the remote it runs `git fetch origin` and checks three things. A refusal at any check exits with code 3:
   - the commit exists on `origin` (otherwise "push it first");
   - the remote checkout has no uncommitted changes;
   - none of your active jobs was submitted from the checkout, judged by the job's submit directory (`squeue %Z`, normalised on the remote with `realpath -m`; a directory that cannot be normalised counts as inside). A job submitted from another directory with `sbatch proj/run.sh` is not detected. `--force` skips this check only.
3. It runs `git merge --ff-only <commit>`, so a diverged remote fails instead of merging. If the directory holds `uv.lock`, it also runs `uv sync --frozen --check`, which checks the environment without changing it. A mismatch exits with code 3 and prints what `uv` said; a remote without `uv` on `PATH` is an error (1), not a mismatch. `rr` never runs `uv sync` itself.

Exit codes: 0 ok, 3 refused, any other code is the remote command's own or an `rr` error (1).

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

- The remote login shell must quote like POSIX `sh` (bash, zsh and sh do; fish does not). Every command runs under `bash -c` with `set -euo pipefail`.
- `sacct` has no `--me`, so `rr acct` passes `-u "$USER"`. `sacct` reports the `--output` pattern unexpanded (`%x-%j.out`). `rr log` expands `%%`, `%A`, `%a`, `%J`, `%j`, `%u` and `%x`, and refuses any other placeholder.
- `rsync` must exist on both machines. Windows has no native `rsync`.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

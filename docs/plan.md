# Plan: implement docs/feature-request.md safely

## Context

`docs/feature-request.md` proposes four groups of features for `rr`: snapshot runs (1), `rr wait` / `--json` / exit codes (2), `rr fwd` (3), recipes and smaller items (4). The code today is small (about 1.4k lines): every remote action goes through `Remote`/`Ssh` in `src/remote.rs`, shell text is built with `quote`/`join`/`script` in `src/quote.rs`, settings come from `src/config.rs`, the guarded fast-forward lives in `src/pull.rs`.

Reading the requests against the code shows two things:

1. Several requests are unsafe or fragile as written (listed per item below, security first).
2. Two security holes already exist in the code and would be made worse by the new features (recipes, `fwd`, snapshots). They must be fixed first.

3. The current structure will not carry the new features: `main.rs::run` is already a 70-line dispatcher that builds shell text inline, and three kinds of side effect (ssh, rsync, git) are run in three different ways. Adding about ten commands to it makes a god function.

Work order therefore changes from the document's "1, 2, 3, 4" to: **R0 stopgap fixes and characterisation tests, R architecture refactor (no behaviour change), A remaining hardening, B item 2, C item 3, D item 1 (after its step 0), E item 4.** The document itself says items 2 and 3 are independent of 1.

Why R0 comes before R: the refactor is about ten commits, and the safety review found that the two exploitable holes (A1 option injection, A2 untrusted project file) would otherwise stay open the whole time. R0 closes them in the current code with the smallest change, and pins the existing guards with tests so the refactor cannot weaken them.

## Phase R0: stopgap fixes and characterisation tests (first, in the current code)

Status: implemented (`host.rs`, `trust.rs`, `sanitize.rs`, `--`/`-s` in `remote.rs` and `transfer.rs`, unit-separator sacct output, pull characterisation tests). Not yet pinned because they need the `Exec` port (R3): the `local_target` fallback test (it now warns on stderr) and the `transfer` probe/limit tests.

Independent small commits, no restructuring:

1. **A1 as written below** (Host validation, `--`, rsync argument protection). It is a few lines in `remote.rs` and `transfer.rs`.
2. **Stopgap trust rule (A2 minimal form):** a project file (`.rr.toml`) is used only after `rr trust`. Every key is covered, including `dir` (see A2). The typed form (`ProjectFile<Untrusted>`) comes in Phase R.
3. **sacct field split bug (exists today):** `logpath.rs:25` uses `splitn(6, '|')`; a job name containing `|` shifts user, workdir and stdout and gives a wrong `tail` path. Use `--delimiter` with the ASCII unit separator and a fixed field count. `-P`/`-p` is required for `--delimiter` to apply.
4. **Characterisation tests that must exist before any code moves** (they pin behaviour the refactor must keep):
   - `decide`: NotOnOrigin is judged before dirty; dirty refuses even when `head == target`; `--force` lifts only the jobs guard; `inside` treats `proj-runs` as outside `proj` and handles a trailing `/` and dir `/`.
   - `pull` flow: exactly two remote calls; a refusal sends no `merge`; `STALE_EXIT` is matched before `0`; the apply script contains `--ff-only` and `--frozen --check`.
   - Fail-closed parsing: a failed state call is an error; incomplete state is an error; a malformed `squeue` job line is an error, never skipped.
   - `ssh_argv` (no test today): host position, the `bash -c` quoting, `-t`. A fake `ssh` on `PATH` that runs the argument through `sh` shows that `$(touch X)` in a command does not execute locally.
   - `local_target`: the fallback to `OriginHead` when git is missing or the directory is not a repository silently ships `origin/HEAD`. Decide the intended behaviour (warn on stderr) and pin it.
   - `transfer`: a failed probe copies nothing; over the limit exits 3 and copies nothing; `put` never passes `--delete`.
   - Stdin: `capture` uses null stdin, `stream` inherits it; a signal-killed child maps to non-zero.

## Phase R: architecture review and refactor (before any feature)

Status: R1 to R10 implemented. Layout: `src/domain/` (pure), `ports.rs`, `app/` (one file per command), `adapters.rs`, `config.rs` and `trust.rs` (file edges), `main.rs`/`cli.rs`. Commands that stream a child's output say what they did first through `ports::Notes`, because that text must precede the output; everything else is an `Outcome` rendered by `domain::outcome::render`. `Clock` exists with a fake and `SystemClock`; `wait` is not built.

### What is already good (keep)

- The `Remote` trait is a real seam and `pull.rs` tests drive it with a stub.
- Decisions are pure data in, data out: `decide`, `parse_state`, `parse_sacct`, `log_path`, `parse_size`, `rsync_argv`, `ssh_argv`, `script`.
- `Target`, `PullDecision`, `Reason`, `PullOutcome` are closed enums; `Commit` is validated at construction.
- `Settings` refuses unknown keys; layering is a pure `over`.

### Findings (each one is a plan item)

| # | Finding | Evidence | Consequence for the new features | Fix |
|---|---|---|---|---|
| R1 | God function. `run` parses config, reads the environment, builds ssh, dispatches 10 commands and holds the exit-code policy. | `main.rs:120-190` | Each new command (`wait`, `status`, `fwd`, `do`, `gc`) grows it; no command can be tested without a real ssh. | One handler per command in `app/`, each `fn(&Session, Args) -> Result<Outcome>`, at most about 30 lines. `run` shrinks to parse, resolve, dispatch. |
| R2 | Remote command text is built inline in the shell layer and is untested: the `squeue`, `sacct`, `scancel` and `tail` bodies. Slurm knowledge is scattered over `main.rs`, `pull.rs` (squeue format) and `logpath.rs` (sacct fields). | `main.rs:147-159,233-241`, `pull.rs:160`, `logpath.rs:6` | `wait`, `status` and `gc` all need sacct/squeue; duplicates would diverge. | New pure `slurm.rs`: field constants, `queue_script`, `acct_script`, `cancel_script`, `tail_script`, `parse_*`, `JobId`, `JobState`. One source of truth for each Slurm fact. |
| R3 | Side effects reach the process directly, outside any seam: rsync (`main.rs:245-267`) and git (`main.rs:193-208`). Only ssh is fakeable. | `Command::new` in `main.rs` | Snapshot mode is mostly local git (`read-tree`, `add -A`, `write-tree`, `archive`) plus streaming over ssh; `fwd` spawns a long-lived child. None of it can be tested now. | One port for local processes: `trait Exec { capture(&Argv, Stdin) -> Captured; stream(&Argv) -> i32; spawn(&Argv) -> Child }`. `Ssh`, rsync and git become pure `Argv` builders executed through `Exec`. `Remote` becomes a thin wrapper that builds the ssh argv. A fake `Exec` records argv and replays output for every feature. |
| R4 | Printing and exit-code policy are mixed into command logic (`report` prints and returns `u8`; `show_log` prints; `transfer` prints and returns `REFUSED`). | `main.rs:211-243,259` | `--json` (item 2) needs the same result rendered two ways. | Commands return `enum Outcome` (data). One pure `render(&Outcome, Format) -> Rendered { code, stdout, stderr }` is the only place that decides text, JSON and exit code. Only `main` prints. |
| R5 | Boolean flags encode policy and state: `pull(.., force: bool)`, `show_log(.., follow: bool)`, `stream(.., tty: bool)`. | `pull.rs:238`, `main.rs:232`, `remote.rs:23` | New flags (`--wait`, `--require-clean`, `--json`, `--open`) multiply them. The project rules forbid booleans for domain state. | `enum JobPolicy { Respect, Override }`, `enum Follow { Once, Follow }`, `enum Terminal { Plain, Tty }`, `enum Format { Text, Json }`. |
| R6 | Stringly typed domain values: `Settings{host,dir,env: Option<String>}`, `Job.workdir`, `JobRecord.*`, `JobId(pub String)`. The "host is required" rule is checked late in `run`. | `config.rs:18`, `main.rs:133` | Phase A needs `Host` validation; item 3 needs `ForwardTarget`; item 1 needs `TreeId`. | Parse at the boundary into `Host`, `RemoteDir`, `EnvFile`. Keep `Settings` as the all-optional file layer and add `Session { host: Host, place: Place }` produced by a pure `resolve(Settings) -> Result<Session>`. `JobId` gets a private field and `parse`. |
| R7 | Duplicated validation and plumbing: the 40-hex check will repeat for `TreeId`; "capture, bail if non-zero with stderr" appears three times; script assembly by `join("\n")` appears in `quote.rs` and `pull.rs`. | `pull.rs:241,259`, `main.rs:235` | Three more copies with snapshot and wait. | One `HexId<const N: usize>` (or `Commit`/`TreeId` over a shared private parser); `Captured::into_stdout(what) -> Result<String>`; one `Script` builder. |
| R8 | `quote.rs` has two responsibilities (shell quoting, and the `Place`/`script` that cd's and sources). `pull.rs` (455 lines) holds ids, state parsing, decision, script text and orchestration. | `quote.rs:16-35`, `pull.rs` | A reason to change for each; merge conflicts as features land in the same file. | Split: `shell.rs` (quote, join), `place.rs` (Place, script), `pull/` with `state.rs` (script and parse), `decide.rs`, `apply.rs` (orchestration), ids to `ids.rs`. Behaviour and tests unchanged. |
| R9 | Time and sleeping are not injectable (needed by `wait` and `fwd --ttl`). | not present yet | A test that really sleeps 30 s is a test nobody runs. | `trait Clock { now(); sleep(Duration) }` with a fake that advances virtual time; `wait` is pure `next_action(&states, elapsed, timeout)` plus a 15-line loop. |
| R10 | Process-wide state is read in the middle of command logic: `std::env::var`, `current_dir`. | `main.rs:126-132` | Hard to test config resolution; `fwd` and `x -s` add more. | Read once in `main` into `Environment { cwd, xdg, home, appdata }`; everything below takes it as a value. |

### Target layout (dependencies point inward only)

```
src/
  main.rs        clap parse, build adapters, call app, print Rendered, exit   (the only printer)
  app/           one file per command; orchestration over ports; returns Outcome
  domain/        pure, no I/O:  ids  shell  place  slurm  pull(state, decide)  logpath
                 transfer(size math)  wait(next_action)  fwd(argv)  snapshot(plan, guards)
                 outcome(Outcome, render)
  ports.rs       trait Exec, trait Clock
  adapters/      OsExec (std::process), Ssh/rsync/git argv builders, SystemClock, file readers
```

Rule: `domain` imports nothing from `app`, `adapters` or `std::{fs,process,env,thread,time}`; `app` imports `domain` and `ports`; `adapters` implement `ports`; `main` wires them. This is a functional core (domain) with an imperative shell (app + adapters), as the project rules require.

### Enforce it, do not just state it

- `clippy.toml`: `disallowed-methods` for `std::process::Command::new`, `std::fs::read_to_string`, `std::env::var`, `std::thread::sleep`, `std::time::SystemTime::now`; `adapters/` and `main.rs` carry a module-level `#![allow(clippy::disallowed_methods)]`. Any I/O added to the core then fails CI.
- `Cargo.toml [lints.clippy]`: `too_many_lines` (threshold 50), `too_many_arguments` (4), `print_stdout`/`print_stderr` denied in the library crate, `unwrap_used`/`expect_used` denied outside tests, `wildcard_enum_match_arm` for closed domain enums (exhaustive handling).
- A test that lists `src/domain/**` and fails if it contains `std::process`, `std::fs` or `println!` (cheap backstop for the lint).
- Conventions for new code: no function over 50 lines; a command handler does at most three things (resolve inputs, call ports, return an `Outcome`); a function that both decides and performs I/O is split.

### Type design: make illegal states unrepresentable, typestate where order matters

Rule of use. Typestate (a value of a different type per stage, stage changes by consuming `self`) is used only where a **safety-relevant order** exists: a step must not run before its guard. Everything else uses plain closed enums, newtypes with a private field and a validating constructor, and non-empty collections. No `PhantomData` marker generics unless two stages share many fields; separate structs are simpler. Privileges are proven by tokens whose constructor is private to the module that grants them. Closed enums are matched without `_` arms so a new variant fails compilation.

**Typestate pipelines**

| Pipeline | Stages (each a distinct type, each method consumes the previous stage) | What becomes impossible |
|---|---|---|
| Pull (existing, `pull.rs`) | `RemoteState` -> `decide(self, JobPolicy) -> Decision::{Refused(Reason), UpToDate(Verify), FastForward(Permit)}`; `Permit { from, to }` has a private constructor in `decide.rs`; `merge_script(Permit)`, `verify_script(Verify)` | Today `apply_script(Option<&Commit>)` can build a merge without any decision; afterwards a merge script cannot be built from a refused or unread state. |
| Snapshot run (item 1) | `BuiltTree` (local, id and file list) -> `ClearedTree` via `clear(self, &Policy) -> Result<_, Refusal>` (secret names, size, symlinks, submodules, dirty policy) -> `ShippedSnapshot` via `ship(self, &Exec)` (full `TreeId`, remote path, provenance) -> `run(&self, Command)` | An unguarded or oversize tree cannot be shipped; a command cannot run in a snapshot that was never shipped; `--require-clean` is a `Policy` value that makes `clear` return `Err` for `Cleanliness::Dirty`, not a check scattered later. |
| Snapshot gc (item 1) | `SnapshotRoot` (validated: non-empty, not `/`, not the home directory) `::child(&SnapshotName) -> SnapshotPath` -> `Unused` (proof built from the resolved squeue paths and the age floor; an unresolvable path yields no proof) -> `Trashed` (renamed to `.trash-*`, re-checked) -> `remove_script(Trashed)` | `rm -rf` of anything outside `<snapdir>/<40 hex>`; removal of a snapshot a job still uses. |
| Project file trust (Phase A) | `ProjectFile<Untrusted>` holds the bytes read once and exposes only its path and hash (what `rr trust` shows); `verify(self, &TrustStore) -> Result<ProjectFile<Trusted>, NotTrusted>` parses those same bytes; only `Trusted` exposes `host`, `env`, `dir`, recipes, forwards | A hostile `.rr.toml` cannot feed any setting into `Session` because the accessors do not exist on the untrusted type. Hash and parse cannot see different bytes. This replaces a runtime check by a type. |
| Forward (item 3) | `ForwardSpec` -> `ListeningForward` via `start(self, &Exec, &Clock)` (port accepted, ssh alive) -> `open_url` exists only here; a `Deadline` is a required field | A URL printed or opened before the port works; a forward with no time bound; a bind address other than loopback (`LoopbackPort(u16)` renders `127.0.0.1:` itself and has no host field). |
| Config (existing) | `Settings` (file layer, all fields optional) -> `resolve(Settings, Environment) -> Result<Session>` where `Session { host: Host, place: Place }` | Code below `main` never sees a missing host; no `.context("no host")` at the point of use. |

**Newtypes and sum types (no typestate needed)**

- `Host`, `RemoteDir`, `EnvFile`, `ForwardTarget`, `LoopbackPort`, `PollInterval` (at least 5 s), `Ttl`, `Timeout`, `ByteLimit`: private field, `parse` constructor, validated once at the boundary (clap `value_parser`, serde `TryFrom<String>`), trusted inside.
- `HexId<N>`: one validated type behind `Commit` (40) and `TreeId` (40); one parser instead of two. `JobId` validated (Phase A).
- `NonEmpty<T>` for job lists: `rr wait` and `rr cancel` with no ids, and `--wait` after a submission that printed no job id, cannot be constructed. `Argv` is `(Program, Vec<Arg>)`, so `argv[0]` cannot panic (`remote.rs:51`, `main.rs:247`).
- `JobState = Active(Pending | Running | Requeued) | Finished(Completed | Failed(FailureKind) | Cancelled | Timeout | OutOfMemory | NodeFail | Preempted) | Unrecognised(String)`: "terminal" is structural, not a method over strings.
- `WaitOutcome = AllCompleted | SomeFailed(NonEmpty<(JobId, FailureKind)>) | TimedOut(NonEmpty<JobId>) | Unknown(Cause)`: success carries no failures, failure carries at least one.
- `TargetOnRemote = OnOrigin(Commit) | Missing` replaces `Option<Commit>`; `JobPolicy = Respect | Override` replaces `force: bool`; `Cleanliness = Clean | Dirty` replaces the dirty flag; `Mode = Checkout | Snapshot`; `RunTarget = Checkout(Commit) | Snapshot(ShippedSnapshot)`; `Follow`, `Terminal`, `Format` replace the other booleans.
- Exit codes: `Exit = Rr(RrCode) | Remote(u8)`. `RrCode` is a closed enum (`Refused`, `Timeout`, `JobFailed`, `Unknown`, `Internal`); a remote command's own code is a different variant, so pass-through can never be mistaken for an `rr` refusal in code (the collision found in Phase B can then only exist at the process boundary, where `--json` carries the distinction).
- Shell text: a `Word` is created only by `quote`; a script body is a list of `Fragment::{Literal(&'static str), Words(Vec<Word>)}`. `&'static str` cannot hold runtime text, so user or remote data can reach a script only through `quote`. `script(place, body)` stops taking `&str`; every `format!` that builds shell text in `main.rs`, `pull.rs` and (later) snapshot code is replaced. This makes shell injection unrepresentable instead of reviewed.
- Provenance and JSON are `Serialize` structs, never `format!`.

**Prove it with the compiler**

- Each pipeline gets `compile_fail` doctests (no new dependency): for example, `merge_script` called with a `Decision::Refused`, `ship` on a `BuiltTree`, `ProjectFile<Untrusted>::host`, `NonEmpty::new(vec![])` used as non-empty, a `Word` built from a `String`. If someone later loosens a type, the doctest starts compiling and the test fails.
- Unit tests then cover only what types cannot: parsing tables, the guard rules, the render function.

### How to refactor safely

Prerequisite: all of R0 is merged (the holes are closed and the characterisation tests exist). Behaviour-preserving, one commit per finding, in this order: R6 + R10 (types and environment, small), R2 (slurm module), R3 (`Exec` port, move rsync and git behind it, add tests that now become possible: the argv of `transfer` and `local_target`), R4 + R5 (`Outcome`, render, enums), R1 (split `run`), R7 + R8 (dedupe and file splits), R9 (`Clock`) when Phase B starts. A3 (sanitising output) is done after R4, because it needs `render` and `stream_filtered`. Replace the R0 stopgap trust rule with the typed `ProjectFile<Untrusted/Trusted>` during R6, with the R0 tests unchanged.

Guards the `Exec` port must not weaken (each has a test): `capture` defaults to null stdin and `stream` to inherited stdin, exactly as today; an exit status with no code (killed by a signal) stays non-zero and is never turned into 0; the fake `Exec` records stdin and tty arguments so tests can see a difference; `main` branches on `PullOutcome`, not on a rendered exit code, so a refusal can never be mistaken for success and run the command. After each commit `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` pass and the CLI surface is unchanged (`rr --help`, exit codes). Existing tests move with their code and keep passing; add characterisation tests for the untested shell text (R2) before moving it.

### How the new features then fit

- Item 2: `slurm::JobState`, pure `wait::next_action`, `app/wait.rs` loop over `Exec` + `Clock`, `Outcome::{Waited, TimedOut, Unknown}` rendered as text or JSON by `render`.
- Item 3: pure `fwd::argv(&ForwardTarget, LocalPort)`, `app/fwd.rs` spawns through `Exec::spawn`, polls with `Clock`, kills the child on the deadline.
- Item 1: pure `snapshot::{plan, guards, provenance}` over facts gathered by a git adapter; `app/run.rs` picks `RunTarget::{Checkout, Snapshot}` and runs one of two pipelines; `Exec::capture` gains a `Stdin` argument for the archive stream.
- Item 4: `Recipe` and its placeholder substitution are pure and produce an `Argv`; `app/do.rs` only runs it.

## Phase A: remaining hardening (prerequisite for every feature)

A1 and the stopgap form of A2 ship in R0 in the current code. Phase A is what remains, and it runs after Phase R so that it lands in the new structure: `Host` and `JobId` are the R6 newtypes, ssh and rsync argv are the R3 builders, A3 needs R4. The A1 and A2 text below is the full specification; R0 implements it minimally and R6 gives it the final types.

**A1. ssh and rsync option injection (high).** `ssh_argv` (`src/remote.rs:32`) puts `host` after the options without `--`. A host that starts with `-` (for example `-oProxyCommand=...`) is read by ssh as an option, which runs a local command. The host can come from a `.rr.toml` that `config.rs` finds by walking up from the current directory, so cloning a hostile repo and running `rr q` is enough. Same pattern in `rsync_argv` (`src/transfer.rs:17`): `{host}:{path}` and local paths are positional with no `--`.
- Add a `Host` newtype (`config.rs` or new `host.rs`): non-empty, no whitespace or control characters, only `[A-Za-z0-9._@-]`, and no leading `-`. **No `:`, `[` or `]`**: a host such as `evil.example:` becomes `evil.example::path` in `main.rs:173`, and rsync then speaks its daemon protocol over TCP to port 873 instead of using ssh. Use an ssh alias for IPv6. In `user@host`, validate the user part and the host part separately, each with no leading `-`. Parse once at the boundary (flag, env, file); `Ssh` holds a `Host`, not a `String`.
- Put `--` before the host in `ssh_argv` and before the two paths in `rsync_argv`. `--` protects only the local parse. The remote rsync also reads a remote path that starts with `-` as an option, so `remote_path` (`transfer.rs:7`) must return an absolute path or prefix `./` when `dir` is unset.
- Add `-s` to rsync so the remote side does not pass `rr get` paths through a shell on older rsync versions (rsync 3.2.4 and later already protect arguments by default; `-s` keeps older clients safe, and a client that rejects it fails closed).
- In the `-e` string (`transfer.rs:20`) quote each element instead of joining with spaces, so a later `ControlPath` containing a space cannot split.
- Tests: `-oProxyCommand=x`, `evil:` and `-a@b` are rejected; argv contains `--` before the host; a local or remote path that starts with `-` is not an option.

**A2. An untrusted project file must not decide where or what runs (high).** `.rr.toml` is honoured from any parent directory, and `env` is `source`d on the remote, `host` picks the machine. README says "keep it out of git", but nothing enforces it. Recipes (item 4) would put command text in the same file.
- Rule: **every key in a project file needs trust, including `dir`.** `dir` is not harmless: it is placed in `{host}:{dir}/{path}` for rsync (`main.rs:173,187`; `dir = "x;touch pwned"` runs a command on the cluster where rsync does not protect arguments) and it chooses where `rr x` and `rr run` execute, so a hostile directory supplies the `pyproject.toml` or script that runs, and `rr put` overwrites files there. Only the user file, flags and `RR_*` variables are trusted without a prompt. `rr trust` stores the canonical path and the SHA-256 of the file in `$XDG_STATE_HOME/rr/trusted` (the direnv model). A changed file is untrusted again and `rr` refuses with a message naming the file.
- Trust-store design: read the file bytes once, then hash and parse those same bytes (a second read is a swap window). No owner or mode check is needed: the digest pins the exact bytes, so a file anyone else can write only becomes untrusted when it changes, and a planted `/tmp/.rr.toml` is refused until the user reviews it with `rr trust`. (Stopping the `ancestors()` walk at `$HOME` is optional polish, not a safety control.) Key the store on the canonical path (symlinks are followed today). Create the store 0600 inside a 0700 directory and refuse it if it is not owned by the user. `rr trust` prints the sanitised file content (A3) before storing the hash, because an ESC sequence can hide lines.
- New small module `trust.rs` (pure `is_trusted(path, hash, store)`, thin file I/O around it) and one new dependency, `sha2`, justified by the hash.
- Alternative considered and rejected: silently ignoring `host`/`env` in project files. It hides the problem and breaks the existing test `project_file_wins_key_by_key_over_the_user_file` without a path forward.
- Tests: an unreviewed file is refused whatever it contains; an edit after trust is refused; hash and parse use one read by construction.

**A3. Untrusted remote text on the terminal (medium).** `rr log` prints job output verbatim. A job (or a dependency it logs) can emit escape sequences (title change, OSC 52 clipboard write). Item 2 adds more such output (`status`, last N log lines).
- Add `sanitize(text) -> String` (pure) that drops ESC and other C0/C1 control characters except `\n` and `\t`, and the bidirectional controls (U+202A-202E, U+2066-2069), so a name cannot be visually reordered. This function is only useful if `rr` sees the text: `Remote::stream` inherits stdio (`remote.rs:63-69`), so `q`, `acct` and `log` currently print remote bytes that never pass through `rr`. Add a line-wise filtered stream (`stream_filtered`) for those three commands.
- Unsanitised sites to route through it: `q`, `acct`, `log`, `eprintln!("rr: {path}")` (`main.rs:239`), `bail!("sacct failed: {stderr}")`, `incomplete remote state:\n{text}`, and the file names in `Reason::RemoteDirty`. All `rr`-authored error text that embeds remote data goes through `render` (R4), which sanitises.
- `rr x` and `rr run` stay raw by design (the user asked for that program's output, and a tty session needs it); say so in the README. Raw bytes for the filtered commands are `rr x -- cat file`.

**A4. Job ids: two types, one fail-closed rule.** Item 2 takes job ids from user input and from a job's stdout, and the pull guard reads them from `squeue`. A single strict regex is wrong for the guard: `squeue %i` prints `123_[1-5%2]` for pending arrays and `123+1` for heterogeneous jobs, so a strict `JobId::parse` in `parse_job` either fails the whole pull or, if unparsable lines are skipped, makes the "jobs from checkout" guard fail open.
- `JobId` (strict, `^[0-9]+(_[0-9]+)?(\.[A-Za-z0-9]+)?$`) is for user input and `--wait` parsing: `cancel`, `log`, `wait`.
- `SqueueId` (lenient, any non-empty token without whitespace) is for the pull guard. `parse_state` turns any line it cannot read into a refusal, never a skip.

**A6. Harden the existing pull guards (not in the draft; found by the safety review).**
- `state_script` proves the target with `git rev-parse -q --verify <sha>^{commit}` (`pull.rs:153-158`). That shows the object exists in the remote store, not that it is reachable from an origin ref (an old fetch, a force-pushed branch). Add `git branch -r --contains <sha>` and require a non-empty answer before `NotOnOrigin` is cleared.
- There are two ssh calls (read state, apply) with a window between them. `apply_script` must re-assert `HEAD == from` and an empty `git status --porcelain` before `git merge --ff-only`, and exit with a distinct "state changed" code that maps to a refusal.
- `inside(workdir, dir)` (`pull.rs:121`) compares strings. Slurm's `%Z` and `pwd -P` can differ through a symlinked home. Compare after `readlink -f` on both sides, and treat any path that cannot be resolved as "in use". Implemented with `realpath -m` on the remote, in the same script. Known gap: `squeue %Z` is the submit directory, so a job submitted from another directory with `sbatch proj/run.sh` is not detected; `%o` is not used.
- The remote shell is assumed to be POSIX at login: `ssh_argv` sends `bash -c '<multi-line>'`, which breaks under csh and changes meaning under fish. Document the requirement, or send the script on stdin to `bash -s`.

**A5. README drift.** Reword the tagline now ("submit, watch" do not exist yet) and again when item 2 lands.

## Phase B: item 2, `wait`, `--json`, `status`

Design points that differ from the document, with reasons:

- **Do not split exit code 3 into 10-13. Keep 3 and add the reason to `--json`.** `rr x` and `rr run` return the remote command's own exit code unchanged (`main.rs:161,165`). A job that exits 3 or 10 is already indistinguishable from a refusal, and new codes make the collision wider. A script that treats a refusal as a job failure (or the reverse) fails open. The reason belongs in structured output.
- **New codes, avoiding collisions.** 0 all jobs COMPLETED; 1 rr internal error (anyhow path, unchanged); 2 is clap usage; 3 refused (unchanged); 4 timeout; 5 a job ended in a state other than COMPLETED; 6 state unknown (ssh or sacct unreachable). The document says "1 otherwise", which collides with the generic error. Avoid 126, 127, 128+n and 255 (shell and ssh meanings).
- **Never read "cannot ask" as "job failed".** ssh exit 255, an empty `sacct` result for a just-submitted id (accounting lag), or a sacct error are not terminal. Retry a bounded number of times, then exit 6. Fail closed.
- **Model states as a closed enum** (the structured form is in the type-design section). Cover every sacct state: PENDING, RUNNING, COMPLETING, SUSPENDED, STOPPED, RESIZING, REQUEUED, SPECIAL_EXIT as active; COMPLETED, FAILED, CANCELLED (also `CANCELLED by 123`), TIMEOUT, DEADLINE, OUT_OF_MEMORY, NODE_FAIL, BOOT_FAIL, PREEMPTED, REVOKED as finished. An unrecognised state is never terminal, so `--timeout` defaults to a finite value (24 h) and `wait` cannot hang forever on a state the table does not know; it is reported as `Unknown` with the raw text.
- **Two code spaces meet in `rr run --wait`.** `rr x` and `rr run` return the remote command's code unchanged, so any `rr` code (1 to 6, 255) can also be a remote code. With `--json` emit `"source": "rr" | "remote"`. In `main`, branch on the `PullOutcome` enum, never on a rendered exit code (`main.rs:163` matches `0 =>` today); `Exec::stream` keeps "signal-killed means non-zero" (`unwrap_or(1)` at `remote.rs:45`).
- **Poll politeness.** `--poll` default 30 s, minimum 5 s, enforced by a `PollInterval` newtype. A tight loop of ssh calls against a shared login node is abusive and can trip fail2ban or the cluster's own rate limits.
- **`rr run --wait` parses stdout of an arbitrary program.** That text is untrusted data. Match `Submitted batch job (\d+)` only through `JobId::parse`, wait for every match, and if none matches exit 6 with a message (do not report success). This needs `stream` to tee stdout (print live and collect); extend `Remote` with that, keep it fakeable.
- **`--json` uses `serde_json`** (new dependency, justified: hand-built JSON from remote text is an injection and escaping risk). Output types are `Serialize` structs, one per command, with a `reason` enum for refusals (`not_on_origin`, `remote_dirty`, `jobs_from_checkout`, `stale_environment`).
- **sacct field separator.** Already fixed in R0 (unit separator with `-P`, fixed field count); `wait` and `status` reuse it.
- `status` log lines pass through `sanitize` (A3); in `--json` they are escaped by serde.

Files: `src/main.rs` (subcommands), new `src/wait.rs` (pure `next_action(states, elapsed, timeout)` plus a small loop over `Remote`), `src/logpath.rs` (delimiter), `src/remote.rs` (tee), `src/pull.rs` (`Reason` already enumerates the refusals).
Tests: a fake `Remote` returning scripted sacct output (PENDING, RUNNING, COMPLETED / FAILED / TIMEOUT / REQUEUED then COMPLETED), timeout, ssh failure mid-wait, empty sacct, job name containing `|`, JSON round trip, control characters stripped.

## Phase C: item 3, `rr fwd`

Keep the size estimate (about 150 lines), with these changes:

- **Forgotten tunnels are the stated problem, so the first version needs a bound.** The document defers the idle timeout, which leaves the motivating risk open. Add `--ttl` (default 8 h, wall-clock; kill ssh at the deadline). True idle detection needs a proxy and is not needed. Remove the "shortcut: no timeout" line from the document.
- **Loopback is not private on a shared workstation.** `127.0.0.1` still lets every local user reach an unauthenticated service such as MLflow. Document this; for command-line clients offer a Unix-socket forward (`-L /run/user/<uid>/rr-<name>.sock:...`, mode 0600) as `--socket`. Browsers cannot use it, so TCP loopback stays the default for `--open`.
- **Fixed local ports.** A pre-bound port is detected by `ExitOnForwardFailure=yes`; keep that. Cookies on `127.0.0.1` are shared across ports, so two forwards on one host share credentials; default to a random free port and use a fixed `local` only when configured.
- **Validate the target.** `ForwardTarget { host, port }` newtype (`host:port`, port 1-65535, host without `-`, `:` or whitespace) and `--` before the ssh host. No wildcard bind: the argv builder hard-codes `127.0.0.1` and the type has no field for a bind address. The forward name is validated `[A-Za-z0-9_-]+` before it enters a socket file name (`rr-<name>.sock`), so `../x` cannot traverse; refuse a stale socket path that exists, and keep the full path under the 108-byte `sun_path` limit.
- **`open` URL.** Do not check a string prefix: `http://127.0.0.1:5051@evil.example/` passes a prefix test. Parse the URL (`url` crate, justified by this check) and compare scheme (`http`), host (`127.0.0.1` or `localhost`), port, and no userinfo. Launch `xdg-open`/`open`/`explorer` as argv, never through a shell.
- Config: `forward` table in the user file only. In a project file it needs the A2 trust.
- **Runner and cleanup.** Spawn ssh, then require `child.try_wait()` is still `None` after a short delay and the local port accepts a connection before printing the URL or opening a browser (a TCP connect alone can succeed against a foreign listener while ssh is exiting). Dropping a `Child` does not kill it and Rust runs no `Drop` on SIGTERM or SIGKILL of `rr`, so "kill on drop" leaves exactly the orphan tunnel this feature exists to prevent. On Linux set `PR_SET_PDEATHSIG` (SIGTERM) on the ssh child in `pre_exec` and install SIGINT/SIGTERM/SIGHUP handlers that kill it; kill on `--ttl` expiry.
- Loopback caveat in the README: besides other local users, any web page in the user's browser can reach a `127.0.0.1` service (CSRF, DNS rebinding), so an unauthenticated UI behind a forward is exposed to that.
Tests as in the document, plus: target and name validation tables, URL parsing (userinfo, other schemes, other ports rejected), ttl kills the fake ssh, killing `rr` kills the fake ssh (Linux), port already taken fails, ssh exiting right after the port opens does not print a URL.

## Phase D: item 1, snapshot mode

**Gate first: step 0 stays as written** (import precedence, `uv sync --frozen --check` from another directory, no hidden sync). Record results with dates in a notes file in the repo. No snapshot code before this passes on the real host. If it fails, keep `--mode checkout` as the only mode for shared-environment runs.

Changes to the proposal, security first:

1. **Do not make snapshot the default yet.** Shipping the working tree by default sends untracked, unreviewed files (a `.env`, a key, a dataset) to a shared cluster filesystem on every run, and drops the "remote runs a commit that is on origin" invariant the README sells. Ship `--mode snapshot` (or a personal `mode = "snapshot"` setting) as opt-in; flip the default only after step 0 and one release that prints a warning. Dry runs are served by the opt-in.
2. **Secret and size guard before anything leaves the machine. Untracked files are opt-in.** A name denylist is bypassable (`secrets.yaml`, `.netrc`, `.npmrc`, `*.p12`, `.pypirc`), so fail closed: tracked files (and tracked changes) ship by default; untracked files ship only with `--include-untracked`, which lists them (first N) and still refuses the denylisted names (`.env*`, `*.pem`, `*.key`, `id_*`, `*credentials*`, `*.kdbx`) unless named with `--allow-path`. Without the flag, an untracked file that the command needs is reported as skipped, so the "no such file" surprise has a visible cause. Enforce the size limit on the tree (`git ls-tree -r -l`) before streaming, not after. Submodules and git-lfs pointer files are not archived as content: refuse them unless `--allow-submodules` (otherwise "no such file" returns by another route). `export-ignore` in `.gitattributes` silently drops files from `git archive`: compare the archive's file count with `git ls-tree` and fail on mismatch.
3. **Full tree id, and the real squatting control is ownership.** Twelve hex digits are 48 bits and make accidental collisions and chosen names easy, so use the full 40-hex (or sha256) tree id as the directory name and display only the 12-hex prefix. A long name alone does not stop squatting: anyone who can write to `<snapdir>` can `mkdir` any name. The control is: `<snapdir>` is `mkdir -m 0700`; on every reuse check `[ ! -L "$d" ]`, owner equals `id -u`, and mode is 700, and refuse otherwise. Create the staging directory with `mktemp -d` (random, 0700) and install `trap 'rm -rf "$tmp"' EXIT HUP TERM`. Run `chmod -R a-w` on the staging directory before the rename, so a killed run never publishes a writable "immutable" tree. Publish with `mv -T` (no-replace rename): a plain `mv` onto an existing directory moves the staging directory inside it as `<tree>/.tmp-x`. If the target exists after a race, treat it as success only after the ownership and mode check passes.
4. **Safe extraction.** Pipe `git archive` into `tar -x --no-same-owner --no-same-permissions` in the temp dir. Before sending, reject symlinks whose target is absolute or contains `..`. `git ls-tree` gives only the mode (120000); read the target with `git cat-file --batch`. (Whether GNU tar already refuses writes through an earlier symlink was not verified; the client-side rule is sufficient on its own and is kept.)
5. **No permanent local refs.** `refs/rr/<tree12>` pins every ephemeral commit forever: unbounded growth, and the secrets from point 2 stay in `.git`. `git archive` and `git diff` accept a tree id directly, so no commit or ref is needed for transport. Create the ephemeral commit only if provenance needs a commit sha, keep it unreferenced, and let `git gc` collect it.
6. **Provenance files are as sensitive as the code.** `.rr/diff.patch` repeats secrets that pass the guard; it lives in the 0700 snapshot, nothing more. Write `provenance.json` from a `serde::Serialize` struct (not format strings).
7. **`rr gc` must not be able to delete the wrong thing.** Accept only direct children of `<snapdir>` whose names match `^[0-9a-f]{40}$` (the `SnapshotPath` type); refuse if `<snapdir>` resolves (`realpath`) to `/`, `$HOME` or empty; `set -u`; never follow symlinks.
   - "In use" means the job's `%Z` is the snapshot or **inside** it (a job submitted from `snap/src` is not equal to the snapshot path), compared after `readlink -f` on both sides (a symlinked home or `pwd -P` versus Slurm path mismatch would otherwise hide every live job). Any path that cannot be resolved counts as in use. The feature-request text "squeue -o %Z equals the snapshot path" is replaced accordingly. The same `inside` flaw is fixed in A6.
   - `flock` is unreliable on Lustre and NFS (a mount without `flock`, or with `localflock`, locks per node only), so do not rely on it. Use: `run` touches `<snapshot>/.rr-used` every time; `gc --older-than` has a floor (1 h) and is required (no "delete everything" default); `gc` renames a candidate to `.trash-<id>`, re-checks squeue, then deletes; if anything errors, keep the snapshot (fail closed).
   - The tree is read-only (`a-w`), which makes plain `rm -rf` fail, so `chmod -R u+w` the renamed `.trash-*` directory first; use `--one-file-system`. Also sweep `.tmp-*` and `.trash-*` directories older than the floor (left by killed uploads).
   - A job that `cd`s elsewhere or copies the tree is invisible to `%Z`; say so in the doc and keep the default retention generous.
8. **Disk quota.** Cap the number and total size of snapshots (`snapshot-limit`, `snapshot-keep`), because a loop of dry runs on a shared filesystem fills a group quota.
9. **`--require-clean` is backwards.** A dirty tree should be explicit: refuse a dirty snapshot when the command is not a dry run unless `--allow-dirty`, and record `dirty` either way. Keep `--require-clean` as the alias for the strict config.
10. **Remote command safety.** The exported `RR_*` values are hex ids and `UV_PROJECT_ENVIRONMENT`/`PYTHONPATH` paths: build the `export` lines with the existing `quote`, never `format!` raw.

Type design (per the project's domain-modelling rules): `TreeId` (validated 40 hex), `SnapshotPath`, `enum Mode { Checkout, Snapshot }`, `enum Cleanliness { Clean, Dirty }` instead of a bool, `enum RunTarget { Checkout(Commit), Snapshot(Snapshot) }`, a separate pure `decide_snapshot` that lists only the snapshot guards, handled with exhaustive `match`. `Remote` gets one more method, `send(script, reader)`, so tests can fake the archive stream.

Tests: tree id stable for equal content; untracked files are not shipped without `--include-untracked` and are reported as skipped; with it, the secret-name guard refuses `.env`; symlink guard (absolute and `..` targets); second run of the same tree transfers nothing; a pre-existing directory with the wrong owner, mode or a symlink is refused; a killed upload leaves no published directory (staging only, swept later); `mv -T` onto an existing tree does not nest; `gc` refuses a snapshot whose path is equal to, or a parent of, a fake `squeue` workdir, an unresolvable workdir, a name outside the 40-hex pattern, a symlinked snapdir, and anything younger than the floor; size limit hit before transfer.

## Phase E: item 4, smaller items

- **Recipes: argv, not a shell string.** `run = ["python", "-m", "train", "{config}"]`, each element quoted by `join`. A `run = "..."` shell string makes `rr do name -- arg` an injection point and invites quoting bugs. Placeholders are typed (`{name}` replaced once, no re-expansion of substituted text; an unknown placeholder is a parse error). `pull = "snapshot|checkout|none"` is a closed enum. Recipes live in `.rr.toml` or the user file, so the A2 trust rule covers them. The tracked `rr.toml` the document proposes is deferred until a second user needs sharing: a committed file that defines commands run on the cluster is remote code execution for anyone who can merge to the repository, and would need the trust prompt to show the command text on change.
- **`rr x -s <script>`.** Fine, but `--interp "python -"` must be split with a shell-words parser into argv (no shell), accepted from flags and trusted settings only. Keep the working directory outside the project as proposed.
- **ssh connection reuse.** Do not force `ControlMaster=auto` on the command line: command-line `-o` beats the user's `~/.ssh/config`, silently overriding a deliberate `ControlMaster no` or a custom `ControlPath`. Opt-in setting `reuse = true`, or apply only when `ssh -G <host>` reports `controlmaster false`. Control path under `$XDG_RUNTIME_DIR` (0700) with `%C` to stay under the 104-byte socket path limit; refuse a directory not owned by the user. Add `ServerAliveInterval` so a dead master does not hang. `rr fwd` opts out as the document says.
- **External subcommands (`rr-foo`):** keep deferred (a `PATH` entry runs with the user's resolved host and directory; fine in cargo's model but add it only with a consumer). If built: pass settings as environment variables, never the trust store or secrets.
- **`rr acct -j <ids>`, `--format`:** ids through `JobId::parse`, format as a closed enum of sacct field sets, not a free string forwarded to `-o`.
- Config schema: because of `deny_unknown_fields`, old binaries reject new files. Add `schema = 1` only if a second breaking key set appears; otherwise state it in the README.

## Documentation

Edit `docs/feature-request.md` in place: remove the idle-timeout shortcut line (item 3), replace the exit-code split (item 2), the default-mode change and the 12-hex id (item 1), and the tracked `rr.toml` (item 4) with the decisions above; remove obsolete text instead of annotating it. Add a short "Security model" section to the README: what `.rr.toml` may set, what is shipped in snapshot mode, loopback caveat for `fwd`.

## Verification

- `cargo test` after each phase; each phase ships with its tests (listed above) in the style already used: pure functions as data, a fake `Remote`, a fake `ssh`/`rsync`/`sacct` on `PATH`, `tempfile` for paths.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
- Manual, phase A: `RR_HOST='-oProxyCommand=touch /tmp/pwned' rr q` must fail with a host error and create no file; a `.rr.toml` containing `env = "/tmp/x"` is refused until `rr trust`.
- Manual, phases B and C against the real host: `rr run --wait`, `rr wait` with a killed ssh (exit 6, not 5), `rr fwd` then `ss -ltn` shows only `127.0.0.1`, ttl closes the port.
- Phase D only after the step 0 results are recorded; test the first snapshot run with a throwaway untracked `.env` in the tree (not shipped; with `--include-untracked` still refused), and a second identical run (no transfer).
- Phase R0 manual check: `RR_HOST='-oProxyCommand=touch /tmp/pwned' rr q`, `host = "evil.example:"` and `dir = "x;touch /tmp/pwned"` in a fresh `.rr.toml` all fail before any process starts, and no file is created.

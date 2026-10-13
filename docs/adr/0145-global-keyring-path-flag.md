# ADR-0145: `--keyring-path`, a global flag for the keyring file

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Accepted.

## Context

`keyring.toml`'s location has exactly one override today: the
`PIGEON_CONFIG_DIR` env var, read inside `Store::default_path()`
(`src/commands/keyring/store.rs:90-97`). There is no CLI flag for it
anywhere in the crate -- pointing `deduplicate`, `rclone copy`/`delete` or
`transform` at a different keyring means mutating process-wide env state.

That matters more than it did when ADR-0022 established the resolution.
ADR-0135 retired the OS keychain, so `keyring.toml` is now the single source
of truth for **every** secret pigeon manages -- IMAP passwords, S3 secret
keys, encryption keys -- as plaintext fields protected only by `0600`. And
ADR-0136/0140 made it load-bearing for `rclone` too: `rclone_config_path()`
(`src/core/rclone/config.rs:36-46`) derives the generated `rclone.conf` from
the keyring file's parent directory, and `core::rclone::execute()` appends
`--config <that path>` to every single rclone invocation
(`src/core/rclone/mod.rs:184-185`). The keyring's location now determines
where pigeon's rclone credentials are written, not just where its own
metadata is read.

Two real defects surfaced while tracing this, both in the code this change
touches anyway, and both fixed here rather than filed:

### 1. `deduplicate` never generates the sandboxed rclone config

ADR-0136 §2 states the generator is "called once at the start of every
rclone-backed job's wizard dispatch". It is called from `rclone/wizard.rs`
(copy and delete) and `transform/wizard.rs` -- but **not** from
`deduplicate/wizard.rs`, whose three data legs are all alias-addressed
rclone calls that read that file:

| Leg | Call site | Command |
|---|---|---|
| Listing | `deduplicate/manifest.rs:151` | `Lsjson` |
| Pull | `deduplicate/worker.rs:204` | per-item `CopyTo` |
| Upload | `deduplicate/worker.rs:742` | bulk `Copy` |

So `deduplicate` ran against whatever a previous `transform`/`rclone` run
happened to leave behind -- or, on a host where neither had ever run, a file
that does not exist. A `pigeon keyring modify bucket` never reached it,
directly contradicting ADR-0136's "regenerated fresh from the current
keyring state every time, so a `pigeon keyring modify bucket` takes effect
on the very next job run with no cache to invalidate".

This was latent rather than observed: the one real deployment provisions
every job on the same host, so `transform` kept the shared file current for
`deduplicate`. `--keyring-path` would have made it observable immediately,
since a run pointed at a fresh keyring directory has nothing to inherit.

### 2. `deduplicate` never checks `rclone` is on `PATH`

The other three dispatches do (`rclone/wizard.rs:175`, `:348`,
`transform/wizard.rs:378`), each with the same "fail immediately on a
missing binary, not partway through a long-running pipeline" comment.
`deduplicate` discovers it at the first `Lsjson` instead.

Both defects exist for the same reason: the preflight block was
copy-pasted into three dispatches and simply never pasted into the fourth.

## Decision

### 1. `--keyring-path` is a root-level global flag

`src/cli.rs`, alongside the four globals that already exist:

```rust
/// Overrides the keyring file's location. Defaults to
/// $PIGEON_CONFIG_DIR/keyring.toml, or the OS-conventional config
/// directory for `pigeon` if that isn't set either (ADR-0145).
#[arg(long, global = true)]
pub keyring_path: Option<PathBuf>,
```

`global = true` is what makes

```
pigeon job run transform --keyring-path /etc/pigeon/keyring.toml --input-file-type mp4
```

parse -- the flag is accepted *after* the subcommand, which is where an
operator naturally types it. A per-job flag on the three job types that
motivated this would have been four copies of the same argument, and would
still have needed process-global state for §2's reason. One global flag
instead covers `deduplicate`, `rclone copy`/`delete` and `transform` as
asked, and `email-sync`/`email-pull` and `pigeon keyring add/modify/delete/
list` for free.

Precedence is **flag > `PIGEON_CONFIG_DIR` > OS config dir**, matching
`metrics::resolve_port` (`src/observability/metrics.rs:14-21`), this
codebase's established shape for an overridable value. The flag names a
*file*; the env var names a *directory* -- the same split
`observability::resolve_log_path` already draws for `--log-file`, so the two
are not redundant and `tests/cli.rs`'s `PIGEON_CONFIG_DIR` isolation keeps
working untouched.

### 2. Resolved once in `main`, into a `OnceLock`

`execute()` resolves the rclone config path itself, deep below any wizard,
from a `&C: RcloneCommand` and nothing else. Threading a path there would
mean changing `RcloneCommand::run(&self)`'s signature and every
`Copy`/`CopyTo`/`Lsjson`/`Purge` call site -- a large diff through code
ADR-0132 consolidated specifically so callers would stop re-deriving
subprocess scaffolding. Instead, follow `observability::LOG_FILE_PATH`
(`src/observability/mod.rs:125`), the existing precedent for exactly this
shape:

```rust
static KEYRING_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_keyring_path_override(path: PathBuf) { let _ = KEYRING_PATH_OVERRIDE.set(path); }
pub fn keyring_path_override() -> Option<&'static Path> { ... }
```

`main` sets it immediately after `Cli::parse()`, before `dispatch`.
`core/rclone/mod.rs:185` and `config.rs`'s call are **unchanged** -- they
resolve a different path without knowing a flag exists. That is the whole
point of the `OnceLock`.

`Store::default_path()` is renamed `Store::resolve_path()`: with two
overrides ahead of the default, the old name named only its last branch.
The same honest-naming reasoning ADR-0136 §3 used reverting `--source-path`
to `--source-bucket`. 13 mechanical call sites; the compiler finds them all.

The precedence itself is extracted into a pure `resolve_path_from(override,
config_dir)`, because a `OnceLock` can only be set once per process and an
in-process test could otherwise exercise only one branch -- ADR-0143's
`circuit_breaker_should_trip` precedent, for the same reason.

### 3. The generated `rclone.conf` follows the keyring file

`rclone_config_path()` keeps deriving from the keyring path's parent, so
`--keyring-path /etc/pigeon/keyring.toml` writes `/etc/pigeon/rclone.conf`.
This preserves ADR-0136's "sibling, one well-known, inspectable artifact"
invariant, and keeps the two files carrying the same plaintext credentials
together rather than splitting them across directories. It also narrows the
race ADR-0136's Consequences named ("two pigeon invocations running
concurrently both rewrite the same file"): two runs against different
keyrings now write different files.

Split into a pure `rclone_config_path_beside(keyring_path)` for the same
testability reason as §2.

### 4. One `load_keyring()`, replacing six copies

Six job-wizard dispatches open with the identical eight-line block. Hoisted
into `shared_wizard.rs`, which already imports `Store`:

```rust
pub(crate) fn load_keyring() -> Result<Store, String>
```

It returns only the `Store`: all six sites bound a `keyring_store_path` that
none of them ever used again.

It **refuses an explicitly-passed `--keyring-path` naming a file that does
not exist**. `Store::load` treats any missing file as an empty store
(`store.rs:100-107`), correct for a first run against the default location
but wrong for a path an operator named outright -- a typo would otherwise
surface much later as `no bucket-config named '...'`, naming neither the
real problem nor the path. That is the same
exit-having-silently-done-nothing failure mode ADR-0140 closed by giving
`Lsjson` its `missing_ok` flag, and the guard is the same answer.

The check lives in `load_keyring` rather than in `Store::load` precisely
because `pigeon keyring add` must still create a keyring at a path that does
not exist yet, so `keyring/wizard.rs`'s six sites keep calling
`Store::resolve_path`/`Store::load` directly.

### 5. One `prepare_rclone()`, fixing both Context defects

The three rclone-backed dispatches share a byte-identical preflight trio.
Hoisted to `shared_wizard.rs`, with `deduplicate` as the fourth call site --
which **is** the fix for both defects, not a separate change alongside it:

```rust
pub(crate) async fn prepare_rclone(store: &Store) -> Result<(), String>
```

-- `check_available()`, then `rclone_config_path()`, then
`write_sandboxed_config()`. Four consumers satisfies this codebase's
duplicate-until-the-third-consumer rule.

In `deduplicate` it is called ahead of the `--upload-only` branch, since
that path's bulk `Copy` is an alias-addressed rclone call too.

Two `check_rclone_available` wrappers (`rclone/worker.rs:40`,
`transform/wizard.rs:297`) reach zero non-test consumers once this lands and
are deleted, `clippy -D warnings` forcing it -- the same mechanical proof of
a complete census ADR-0131/0136 relied on. This closes the leftover ADR-0132
§1 identified ("the five duplicate `check_rclone_available` copies collapse
into `core::rclone::check_available()`") but only partly delivered.
`rclone::worker`'s better error text is preserved in `prepare_rclone`,
generalized from "required for 'pigeon job run rclone'" to cover all four
jobs; `rclone/worker.rs`'s three test guards call `check_available()`
directly.

## Consequences

- Every pigeon invocation can be pointed at an arbitrary keyring without
  touching process env state, which is what makes running two differently-
  configured pipelines on one host practical.
- **`--keyring-path` names a file whose *directory* pigeon also writes to**,
  since §3 puts the generated `rclone.conf` beside it. An operator pointing
  the flag at a read-only path, or at a directory they expect pigeon only to
  read, will be surprised. Stated here rather than left implicit: both files
  carry the same plaintext credentials (ADR-0135), and both are `0600`.
- `deduplicate` now regenerates the sandboxed config on every run, so a
  `pigeon keyring modify bucket` reaches it. Its credentials were previously
  whatever the last `transform`/`rclone` run wrote -- correct in the one real
  deployment, by coincidence of co-location rather than by design.
- `deduplicate`'s CLI tests now require `rclone` on `PATH`, since its
  dispatch gains the preflight check. `transform`'s tests already required
  both `rclone` and `ffmpeg`, so this is an existing condition extending to
  one more job, not a new class of requirement -- and CI runs only
  `fmt-check`/`lint` (ADR-0119), with the test suite gated locally by
  `mise run ci`.
- `Store::default_path` is renamed with no deprecated alias. It is
  `pub`, so this is technically a breaking change to the library surface;
  `pigeon-cli` is published as a binary crate whose library half has no
  documented external consumers, and this codebase does not keep migration
  shims (ADR-0017, ADR-0094, ADR-0096, ADR-0103, ADR-0110, ADR-0114).
- A second `--keyring-path` passed later on the same command line is
  silently ignored (`OnceLock::set` fails after the first). Clap resolves
  repeated occurrences of a non-repeatable flag to the last one before
  `main` ever sees it, so this is unreachable in practice.

## Out of scope

- **A `--config-dir` flag.** `PIGEON_CONFIG_DIR` already covers the
  directory case, and the Docker image sets it (`Dockerfile:30`). The flag
  deliberately addresses the file, the thing the env var cannot name.
- **A `PIGEON_KEYRING_PATH` env var.** Redundant with `PIGEON_CONFIG_DIR`
  for every case that matters; a second env var reading the same file would
  add a precedence rule with no use behind it.
- **Any change to `PIGEON_SECRET_<ALIAS>`** (ADR-0087). It overrides a
  secret's *value*, orthogonal to where the file lives.
- **Encryption-at-rest for either file.** Unchanged from ADR-0135's
  reasoning: there is nowhere safe to keep a master key that would not
  recreate the problem one level down.
- **Per-job keyring paths within one invocation.** The `OnceLock` is
  process-wide by construction. No use case exists -- a job run addresses
  one keyring.

## Amends

ADR-0022 (its `PIGEON_CONFIG_DIR`-only resolution gains a higher-precedence
flag, and `Store::default_path` is renamed `resolve_path`), ADR-0135
(`keyring.toml`, now holding every secret, is relocatable per invocation),
and ADR-0136 (§2's "called once at the start of every rclone-backed job's
wizard dispatch" was false for `deduplicate` and is now true; the generated
config's location becomes operator-controlled; §1's `check_rclone_available`
consolidation is completed). Each gets a short forward-pointing "Amendment"
section, per this repo's convention for amending a merged ADR without
rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `cargo clippy --all-targets --all-features -- -D warnings` clean after the
  two `check_rclone_available` wrappers are deleted -- the mechanical proof
  the zero-consumer census was complete.
- New unit tests in `src/commands/keyring/store.rs`: `resolve_path_from`
  prefers an explicit path over `PIGEON_CONFIG_DIR`, falls back to the env
  var when there is no override, falls back to the OS config dir when
  neither is set, and uses an explicit path verbatim without appending a
  file name (the file-vs-directory distinction).
- New unit tests in `src/core/rclone/config.rs`:
  `rclone_config_path_beside` lands `rclone.conf` next to the keyring, for
  both a conventional and an arbitrarily-named keyring file.
- New tests in `tests/cli.rs`: `--keyring-path` appears in `pigeon --help`
  and in all four rclone-backed `job run ... --help` outputs (proving
  `global = true`); an explicit path wins over `PIGEON_CONFIG_DIR`; a
  nonexistent explicit path fails with code 1 naming the path *and* without
  the misleading "no bucket-config named" text; `pigeon keyring list` reads
  the overridden path; and `deduplicate` generates `rclone.conf` beside the
  keyring it was pointed at, leaving the default config dir untouched --
  the regression test for Context defect 1.
- No test reaches a real rclone transfer. A fixture bucket-config uses an
  unroutable loopback endpoint, and every test asserts a failure that occurs
  before any transfer: rclone's `--retries 5 --low-level-retries 20` turns
  even a refused connection into minutes of backoff, which is how an earlier
  draft of these tests took the suite from 1.2s to over two minutes.
- Manual, against a real deployment: `pigeon keyring add bucket
  --keyring-path /tmp/alt/keyring.toml`, then `pigeon job run transform
  --keyring-path /tmp/alt/keyring.toml --input-file-type jpg`, confirming
  `/tmp/alt/rclone.conf` is generated and used and that the OS config
  directory is never written to.

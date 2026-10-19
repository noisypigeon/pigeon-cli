# CLI-0158: encode `soffice`'s URL arguments, kill it on timeout, and fold Unicode normalization into collision detection

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-17.
- **Status**: Accepted.

## Context

The first real `job run transform --input-file-type docx` (`source:docx/` →
`destination:pdf/`, `--concurrency 8`) ran twice into the same
`--local-output`:

- **Run 1** (`18df7ad2e8e5a640-pigeon-10bff`) pulled 370 files and pushed 72.
  It failed **298** at transcode and **1** at pull, and exited 1 after 5m31s.
- **Run 2** (`18df7b2f12b4b1d0-pigeon-11589`) skipped the 72 already done. It
  then tripped CLI-0121's circuit breaker on its first 20 completions, and one
  in-flight file held it open for another five minutes before it exited.

The failures fall into four groups:

| # | Errors | Root cause |
|---|---|---|
| A | 297 × `soffice produced no usable .pdf … (exit None) … libc++abi: terminating due to uncaught exception of type com::sun::star::uno::RuntimeException` | `-env:UserInstallation=file://{path}` is built from a raw, unencoded path |
| B | 1 × `soffice timed out after 300s` | The same, with a `#` in the path. The timed-out `soffice` was then **never killed** |
| C | 1 × pull failure, `Solutions S Montréal.docx` | Two source keys differing only in **Unicode normalization** collide on APFS |
| D | All 297 crashes are reported as `-- is not a readable document` | Every `soffice` failure gets the caller's wrong-input signature, including a crash |

### A and B: the profile directory is a URL, and nothing encodes it

`soffice::convert` (CLI-0149, generalized by CLI-0154 §6) gives each
invocation its own profile at `<work_dir>/lo-profile`. It passes the profile
as `format!("-env:UserInstallation=file://{}", profile_dir.display())` and
the output directory as a plain `--outdir <work_dir>`. `work_dir` is the
per-file `ScratchDir`, named after the file's `group_key`, and for every
non-spreadsheet kind that key starts with the **raw source stem**
(CLI-0151 narrowed only spreadsheets to `strict_stem`). So every source
filename ends up inside a URL that is never percent-encoded.

The run's own data shows the split exactly. **All 72 files that converted
have neither a space nor a `#` in their name, and all 298 that failed
transcoding have one or the other.** Content had nothing to do with it:
`Lore Feedback.docx`, one of the 297, converts serially in 1.3 s into a
24 KB PDF. Running 40 of the failed files 8-way parallel into hex-named
directories converts all 40. Concurrency is not the cause either.

macOS kept 33 crash reports from the run window
(`~/Library/Logs/DiagnosticReports/soffice-2026-10-17-1817*.ips`), each with
parent process `pigeon` and the same faulting stack: `SIGABRT` from
`__cxa_rethrow` in `configuration::ReadWriteAccess::create`, called from
`desktop::Desktop::Init`. That is soffice failing to open its own profile's
configuration before it ever looks at the document.

Converting one known-good `.docx` with a profile directory named after each
character class pins it down:

| Profile directory | Result |
|---|---|
| `plain` | exit 0, PDF |
| `accent-é`, `colon:x` | exit 0, PDF |
| `with space` | **exit 134, the run's exact `RuntimeException` abort** |
| `paren (x)` | **exit 134, same** |
| `hash#218` | **hangs indefinitely** (`#` starts a URL fragment) |
| `pct%41` | exit 0, but the profile is created at `pctA` and **no PDF is written**: `--outdir` is URL-decoded too |

Passing the **input file** with a space, `#`, `%` or `:` in its name
converts correctly every time. Only the two *directory* arguments are read
as URLs. Passing both as properly percent-encoded `file://` URLs fixes every
case above, `pct%41 x#1 (y)` included.

Group B is the `#` row. `2024-01-26_RE_[EXTERNAL]_…_(#1-75138352145)_1.docx`
hung, and `tokio::time::timeout` reported it after 300 s. But dropping a
`Command::output()` future does **not** kill the child, because tokio only
does that with `kill_on_drop(true)`, which was never set. Both runs' hung
`soffice` processes were **still running** 25 and 31 minutes later, each
holding a scratch profile. That is also why run 2 sat with one file in
flight for five minutes after the breaker had already decided.

This is a latent bug in CLI-0149 too. `.numbers` goes through the same
runner, and CLI-0149's only real run happened to use `.xlsx`, which never
touches `soffice`.

### C: case is not the only thing APFS folds

`pull-files-from.txt` lists `Solutions S Montréal.docx` **twice**: once with
`é` decomposed (NFD, `e` + U+0301) and once composed (NFC, U+00E9). Those
are two distinct S3 keys. APFS matches names regardless of normalization,
so locally they are one file, and rclone resolved the clash itself. It
logged `Duplicate object found in source - ignoring` at `notice` level and
dropped one, exactly the failure CLI-0144 traced for case collisions. CLI-0130's
reconciliation then correctly reported it as undelivered.

CLI-0152/0156 route colliding keys through per-slot pulls, but they detect a
collision with `to_lowercase()` alone, so a normalization-only pair never
reaches that path.

### D: a crash is reported as the file's fault

`soffice::convert` appends its caller's signature (`is not a readable
document`, or `… spreadsheet` for `.numbers`) to every failure. On a signal
death (`exit None`) that is false: the file was never read. It also has
consequences. CLI-0143 treats that signature as **wrong-input-kind** and
deletes the local copy, and the breaker's explanation steers the reader
toward "wrong `--input-file-type`".

## Decision

### 1. Pass both directory arguments as encoded `file://` URLs (A, B)

`soffice::convert` builds `UserInstallation` and `--outdir` with
`url::Url::from_directory_path`, which percent-encodes every reserved
character. `url` is already in the dependency tree (2.5.8, via `async-native-tls`)
and becomes a direct dependency. A path that cannot be expressed as a URL
(a relative path, which this job never passes) fails that file with a
clear error instead of reaching `soffice`.

The scratch directory **keeps** its source-derived name. It is what makes a
retained failure findable on disk, and the bug was the missing encoding,
not the name. Renaming it would also leave every other URL-sensitive
argument one future change away from the same mistake.

### 2. Kill `soffice` when its timeout fires (B)

`command.kill_on_drop(true)`. When `SOFFICE_TIMEOUT` wins the race, the
dropped future now kills the process instead of orphaning it. This is the
only converter call wrapped in a timeout. Every `ffmpeg`, `pandoc` and
`ebook-convert` call runs to completion, so none of them has the same
exposure.

### 3. Fold normalization as well as case (C)

The four `to_lowercase()` sites that decide "these collide locally" are
`case_insensitive_collisions`, `case_collision_groups`, and the slot and
prefix checks in `assign_case_variant_slots`. They become one
`local_fold(key)`: **NFC, then lowercase** (`unicode-normalization`, a new
direct dependency). A normalization-only pair then takes the same per-slot
pull CLI-0152/0156 give a case pair. Both keys arrive, and
`compute_destination_name`, which hashes the key's exact bytes, keeps them
distinct at the destination.

NFC-plus-lowercase **approximates** APFS's matching rather than reproducing
it: APFS uses its own Unicode case-folding tables. Any pair the approximation
misses still shows up exactly as before, as an undelivered file with
rclone's duplicate notice. This rule cannot make a collision silent. The
pre-pull notice now says the keys "differ only by letter case or Unicode
normalization".

### 4. Report a crash as a crash (D)

When `soffice`'s exit status has no code, meaning it died by signal, the
error reads `soffice crashed (signal N) converting <file> --
converter crashed`. It carries stderr and **not** the caller's wrong-input
signature. A new `CONVERTER_CRASHED_SIGNATURE` goes into
`NON_RETRYABLE_SIGNATURES` but **not** into `is_wrong_input_kind_error`,
for CLI-0143's reason that the two answer different questions:

- **Retrying won't help** on a deterministic crash, and a systemic one like
  this run's should still trip the breaker. So it is non-retryable.
- **The file isn't at fault**, so its local copy is kept for diagnosis under
  CLI-0124's retain-on-failure rule rather than deleted. So it is not
  wrong-input-kind.

The breaker's explanation adds a third cause, the converter itself
crashing, and names this ADR.

## Consequences

- The 298 failures from these runs convert on the next `docx` run. They have
  no destination object, so CLI-0124's pre-pull filter selects them again
  without any intervention.
- `doc`/`pages`/`rtf`/`pptx`/`key`/`numbers` are fixed too. They share the
  runner and had the same exposure.
- A timed-out conversion no longer leaks a process.
- A source key pair that differs only by normalization is pulled in full
  instead of losing one member.
- An `soffice` crash keeps its local copy. On a run where it is systemic,
  that is bounded by the breaker tripping after 20 completions.

## Out of scope

- **Other converters' arguments.** `ffmpeg`, `pandoc` and `ebook-convert`
  take plain filesystem paths, which the input-file row above shows are
  safe. Nothing else in this job builds a URL. Permanent unless that changes.
- **Remediation.** Nothing needs it: the files were never pushed, and a
  rerun picks them up.
- **The orphaned processes left by these two runs.** They predate this fix
  and are cleaned up by hand, once.

## Amends

CLI-0149 and CLI-0154 (the `soffice` runner's arguments and its
failure-message contract), CLI-0143 (a converter crash is not
wrong-input-kind), and CLI-0144, CLI-0152 and CLI-0156 (what counts as a
local collision).

## Verification

- `mise run ci` clean.
- Pure tests:
  - the encoded `UserInstallation`/`--outdir` values for a directory containing a space, `#`, `%` and `(`;
  - `local_fold` treating an NFD/NFC pair as one, plus `case_insensitive_collisions` detecting it and `assign_case_variant_slots` separating it;
  - a crash message being non-retryable but not wrong-input-kind, and the existing wrong-input signatures unchanged.
- `soffice`-gated test: an RTF fixture converts from inside a work directory named `a b#1 (c)%41`. Before this ADR that directory crashes, hangs, or writes elsewhere, depending on the character.
- `kill_on_drop` is asserted by construction, not by a test. Exercising it needs a conversion that reliably outlives a timeout, and the one reliable hang this ADR knows of is the bug §1 fixes.
- Manual: rerun the same `docx` command and expect ~298 more transcoded, no new
  `soffice-*.ips` crash reports, the `#` file converted, both `Montréal`
  keys pulled through a slot, and no `soffice` process left after exit.

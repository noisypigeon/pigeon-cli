# ADR-0078: standard procedure for analyzing job-run logs, packaged as a skill

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-28.
- **Status**: Accepted.

## Context

ADR-0073 gives every `pigeon` command a durable JSONL log
(`~/Library/Application Support/pigeon/logs/pigeon.jsonl` by default), plus
resource sampling and a panic hook -- but it only specifies *what gets
logged*, not *how to read it*. In practice, the log has already been the
deciding evidence exactly once: diagnosing a real `pull-transform` SIGKILL
(ADR-0076) meant hand-writing ad hoc `jq`/Python one-liners to isolate the
crashed run from a shared, append-only, multi-invocation file, pull its
`resource_sample` stream, and correlate a 6.2GB RSS spike with ~68GB of
disk I/O to conclude "OS-level swap thrashing." That process worked, but
it was reinvented from scratch under time pressure, and every fact about
the log's actual shape (field names, per-job `step` vocabulary, the
`resource_sample` shape, the *absence* of any run-ID/PID field) had to be
rediscovered by re-reading source.

This ADR formalizes that procedure once, as a **documented, repeatable
analysis method with a concrete takeaway format** -- and packages it as a
Claude Code skill (`.claude/skills/analyze-job-run/SKILL.md`) so it's
invoked automatically on the same kinds of requests that triggered the
ADR-0076 investigation ("can you read the log dump and tell me what went
wrong"), instead of being re-derived by fresh exploration every time. This
ADR is deliberately **no-code**: it changes no Rust source, adds no new
`pigeon` subcommand or flag. The log-reading discipline stays a documented
procedure a human or an AI assistant follows, not new product surface.

## Decision

### 1. A new skill: `.claude/skills/analyze-job-run/SKILL.md`

Follows this repo's one existing precedent (`.claude/skills/release-pr/SKILL.md`):
plain two-field frontmatter (`name`, `description`), an H1 title, a short
"why this discipline matters" intro, then a numbered, prescriptive
`## Steps` procedure with real commands in fenced code blocks and rationale
prose alongside each step -- not just a terse checklist.

The skill covers, in order: locating the log (default path plus the
`--log-file`/`PIGEON_LOG_DIR` overrides); isolating one run from an
append-only, interleaved, multi-invocation file that carries no run-ID or
PID field (the documented workaround: filter by the outer span's `command`
field, then bound by timestamp, treating a run's log simply stopping
mid-stream -- no closing `"command finished"`/`exit_code` pair -- as itself
the crash signature, since SIGKILL can't be caught); establishing the
timeline; pulling the `resource_sample` stream and reading its
CPU/mem/disk-I/O fields for memory-pressure or swap-thrashing signatures;
pulling WARN/ERROR events grouped by each job's `step` field and
cross-checking the tally against that job's own printed summary line;
checking for a panic-hook entry; and writing the takeaway in a fixed shape
(plain-English summary, root-cause hypothesis with a stated confidence
level, evidence citations quoting actual log lines -- never an unsupported
assertion -- and a concrete next action, including offering to write a
follow-up ADR when the takeaway reveals an actionable gap, exactly how the
ADR-0076 investigation began). The skill also bakes in a field/vocabulary
cheat sheet (every top-level JSONL field, the `command_name()` values, the
real per-job `step` vocabulary, and an explicit note that keyring commands
emit no per-operation tracing at all) so this doesn't need re-deriving from
source every time, plus a stated limitations section (no disambiguation
between two concurrent runs of the same command; very fast crashes may
have 0-1 resource samples; a note to keep the cheat sheet updated in the
same PR that changes the logging code).

### 2. This ADR stays short

The exhaustive step-by-step procedure and cheat sheet live only in the
skill file, not duplicated here -- matching how `release-pr` already exists
without a dedicated ADR spelling out its steps a second time.

## Consequences

- No Rust code, no new CLI surface -- `mise run ci` passes trivially
  (nothing changed under `src/`).
- Future schema changes to the JSONL log (a new tracing field, a new
  per-job `step` value) should update this skill's cheat sheet in the same
  PR, or the skill quietly goes stale -- called out explicitly in the
  skill's own "Known limitations" section as a maintenance reminder, not
  left implicit.
- Both changelogs get an entry, following the observed precedent of
  ADR-0052 (also a no-code, cross-cutting decision) appearing in both
  `CHANGELOG.md` and the root log -- root entry scoped
  `[repo]` per ADR-0050's guidance for non-crate-specific ADRs.

## Out of scope

- Adding an actual run-ID/PID field to the log schema to fix the
  run-isolation gap -- a real code change, deliberately not bundled into
  this no-code ADR; revisit as its own ADR if the timestamp-proximity
  workaround proves too fragile in practice.
- A `pigeon logs` subcommand or any other tooling automation -- this stays
  a documented procedure a human/AI follows by hand with `jq`, not a new
  product feature.
- Log rotation/retention policy -- already an explicit ADR-0073 "out of
  scope" item, untouched here.

## Verification

1. `mise run ci` stays clean (no source changes).
2. Dry run: point the skill at a real or synthetic `pigeon.jsonl`
   containing at least one clean run and one run that stops mid-stream
   (simulating a crash); confirm the procedure correctly identifies which
   is which and produces a takeaway in the specified shape.

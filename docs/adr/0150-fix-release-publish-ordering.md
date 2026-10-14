# ADR-0150: publish after pushing, not before — a merge racing the release burns a version

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-14.
- **Status**: Accepted.

## Context

Every merge to `main` since ADR-0147 has failed its release run. Five
consecutive red runs, and `main` could not publish at all.

The cause is not a stale version bump. Run `38488459828` (the ADR-0147
merge, commit `4b066e9`) checked out `main`, spent 2m50s building,
**published `pigeon-cli@0.4.28` successfully** at 00:11:39, and six seconds
later hit:

```text
   Published pigeon-cli v0.4.28 at registry `crates-io`
To https://github.com/noisypigeon/pigeon-cli
 ! [rejected]        HEAD -> main (fetch first)
error: failed to push some refs to 'https://github.com/noisypigeon/pigeon-cli'
hint: Updates were rejected because the remote contains work that you do not
      have locally. This is usually caused by another repository pushing to
      the same ref.
```

PR #89 merged while that run was still building, so by the time
`release.sh` reached `git push origin HEAD:main`, `HEAD` was no longer a
fast-forward of `main`. `release.sh` publishes **before** it pushes, so the
version was already gone when the push failed.

The resulting state:

- crates.io has `0.4.28` (confirmed against the sparse index).
- `main` is at `0.4.27`, and `v0.4.28` exists as a tag **nowhere** — not
  locally, not on the remote.
- Every subsequent run recomputes `0.4.27 + 1 = 0.4.28` and dies on
  `error: crate pigeon-cli@0.4.28 already exists on crates.io index`.
  Confirmed as the failure for runs `38488702618`, `38492304897`, and
  `38492523764`.

So the failure is **unrecoverable** (a crates.io version cannot be
reclaimed) and **self-perpetuating** (every later run collides on the same
number). That combination, not the lost race itself, is what took the
pipeline down for five merges.

### The ordering was deliberate, and its reasoning needs answering

This is not an oversight to quietly correct. ADR-0117 chose this ordering
explicitly and argued for it:

> **Publish runs before the commit/tag/push, not after.** If `cargo
> publish` fails (network blip, registry outage, a transient verification
> failure), `main` stays untouched and simply re-running the job is a safe,
> idempotent retry. Pushing the bump to `main` first and having publish
> fail afterward would leave `main` showing a version crates.io never
> received, and a naive retry would double-bump since `Cargo.toml` on
> `main` would have already moved.

That reasoning is sound for the failure it considers — a *failed publish*.
It simply never considers the inverse: a **successful publish followed by a
failed push**. And the inverse is strictly worse than the case the ordering
protects against, on both axes. A failed publish is recoverable by
re-running. A burned registry version is not recoverable at all, and it
wedges every future release.

ADR-0117's stated *cost* of the other ordering also does not survive
contact with its own rules. It rejects push-first partly because "a naive
retry would double-bump since `Cargo.toml` on `main` would have already
moved" — but ADR-0117 §2a already decrees that the bump happens

> always, unconditionally, regardless of how that version got there … it
> simply operates off that new baseline on the next merge, no
> special-casing required.

A skipped version number is therefore something ADR-0117 has already
decided it does not care about. That asymmetry is this ADR's whole
argument: **a hole in the version sequence is free; a burned registry
version takes the pipeline down.**

## Decision

### 1. Push before publish

`release.sh` commits, tags, and pushes first, and publishes only after
that succeeds.

A rejected push now costs nothing. Nothing has been published, `main` is
untouched, the on-disk bump dies with the runner, and the accumulated
`CHANGELOG.md` `[Unreleased]` bullets simply roll into the next merge's
release. The `concurrency: release-main` group already queues runs, so the
next merge's run starts from a `main` that includes the merge which won the
race and cuts the release cleanly.

The push is deliberately **not** `--force-with-lease`. A rejection is
precisely the signal this ADR wants, and forcing would clobber the
concurrent merge that caused it.

The residual exposure inverts: push succeeds, publish fails, leaving a
tagged version crates.io never received. Per ADR-0117 §2a's no-history
rule, the next merge cuts the *following* patch rather than retrying that
one, so recovering the skipped version means a manual `cargo publish` at
that tag. Named here as a known, bounded cost — it is one hand-run command
against a tag that exists, not a wedged pipeline.

### 2. `--allow-dirty` goes away

ADR-0117 needed that flag only *because* of the ordering it chose:

> Because the version bump happens on disk before publish and is only
> committed after a successful publish, the working tree is guaranteed
> dirty at publish time — `cargo publish` therefore runs with
> `--allow-dirty`.

Reversing the order makes the tree clean at publish time, so the flag is
simply dropped. It existed to suppress a real safety check; nothing now
needs it suppressed. A second, independent benefit of §1, and the reason
§1 is a simplification rather than a trade.

### 3. Publish becomes idempotent, explicitly

Before publishing, `release.sh` queries the crates.io sparse index for the
computed version:

```text
curl -fsSL https://index.crates.io/pi/ge/pigeon-cli
```

One newline-delimited JSON record per published version; no auth, no
tooling beyond `curl`, and no registry mutation. If the computed version is
already present, the publish is **skipped with a warning** rather than
failing the run.

This gives ADR-0117 the property it wanted from its ordering — "re-running
the job is a safe, idempotent retry" — as an **explicit check** that holds
under *either* order, instead of as a side effect of one. It is also, on
its own, what would have un-wedged the pipeline: the four runs after the
lost race would each have skipped the publish, pushed their own release
commit, and carried on.

A failure to reach the index is **not** treated as "not published". The
check is skipped and the publish attempted, letting `cargo publish`'s own
error be the authority — a network blip must not silently suppress a real
release.

### 4. Reconcile the current state by skipping `0.4.28`

`Cargo.toml` goes to `0.4.29`, so the next merge cuts `0.4.30`. `0.4.28`
stays published on crates.io, untagged, with no GitHub release and no
`CHANGELOG.md` section.

The alternative — retroactively tagging `4b066e9` as `v0.4.28` and moving
ADR-0147's changelog bullet into a dated section — would make git agree
with the registry, but it manufactures a release record after the fact for
a run that failed. Skipping is what ADR-0117 §2a already licenses, and this
ADR is where the hole gets **explained** rather than closed:

- `0.4.28`'s published artifact is commit `4b066e9`'s tree (ADR-0147) with
  its version bumped.
- It is **not yanked.** It is not broken — it carries two unused
  dependencies ADR-0148 later pruned, which is bloat, not breakage, and a
  yank would signal to anyone watching that it was unsafe to use.
- `cargo install pigeon-cli --version 0.4.28` therefore works and
  corresponds to `4b066e9` plus a version bump, which is recorded here
  because it is recorded nowhere else.

### 5. A rejected push fails the run, loudly

A rejected push exits non-zero with a message naming the cause and the
remedy:

```text
Push to main was rejected -- a merge landed while this release was
building, so this release is deferred. Nothing was published and main is
unchanged; the next merge will cut it.
```

Exiting 0 was the alternative, and is tempting: under §1 the outcome is
genuinely benign and self-healing, and a red run on every concurrent merge
trains an operator to ignore red runs. It is rejected because a deferred
release is a real deviation, and this repo has twice been bitten by
treating a benign-*looking* no-op as success — ADR-0139's source filter
that matched nothing and exited 0, and ADR-0140's unresolvable source that
pulled nothing and exited 0. Both cost multiple runs to notice. The
consistent posture is to make the silence loud.

## Consequences

- **`main` can publish again.** The immediate effect; five merges' worth of
  `[Unreleased]` entries (ADR-0147's follow-up through this one) release
  together as `0.4.30`.
- **A lost race now defers a release instead of destroying one.** The
  failure mode becomes "this merge didn't release, the next one will",
  which is self-healing, from "a version is burned and no merge can ever
  release again", which was not.
- **`cargo publish` runs against a clean tree** for the first time, so its
  uncommitted-changes check is live rather than suppressed.
- **The version sequence has a permanent hole at `0.4.28`**, explained in
  §4 and nowhere else. Anyone reading `git tag` will find `v0.4.27`
  followed by `v0.4.30`.
- A publish that fails *after* a successful push now leaves a tagged
  version missing from crates.io. Bounded and hand-recoverable (§1), and
  strictly preferable to the reverse.
- The race itself is **not** eliminated — a merge can still land mid-run.
  This ADR makes the consequence cheap rather than preventing the cause;
  genuinely preventing it would mean serializing merges against release
  runs, which is a far larger change for a single-contributor repo where
  the collision needs two merges inside one ~3-minute window.
- `pigeon`'s release pipeline now depends on `index.crates.io` being
  reachable for the pre-check — degraded gracefully per §3, so an
  unreachable index costs nothing beyond losing the idempotency skip.

## Out of scope

- **Preventing the race**, as distinct from making it cheap (see
  Consequences).
- **Automatically publishing a tagged-but-unpublished version** on a later
  run. That needs the release to look at history, which ADR-0117 §2a
  deliberately refuses; §1 keeps that refusal and documents the manual
  recovery instead.
- **Retroactively tagging `0.4.28`** or creating its GitHub release — the
  rejected alternative in §4.
- **Yanking `0.4.28`** — it is not broken (§4).
- Reverting ADR-0118's component install or ADR-0119's narrowed precheck;
  both are orthogonal and unchanged.

## Amends

ADR-0117, which chose publish-before-push and introduced `--allow-dirty`
as a consequence of that choice. Both are reversed here, with ADR-0117's
own §2a supplying the argument for why its stated cost of the other
ordering does not apply. Its version-bump rule, changelog cut, concurrency
group, loop guard, and GitHub Release step are all unchanged. ADR-0117 gets
a short forward-pointing "Amendment" section, per this repo's convention
for amending a merged ADR without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `bash -n .github/scripts/release.sh`.
- `mise run release -- --dry-run` locally: bumps `Cargo.toml`/`CHANGELOG.md`
  on disk, reports the index pre-check's verdict for the computed version,
  runs `cargo publish --dry-run`, and makes no commit, tag, push, or
  publish. Discard with
  `git checkout -- Cargo.toml Cargo.lock CHANGELOG.md`.
- The index pre-check, exercised against both answers directly: `0.4.28`
  reports present (the version this ADR exists because of) and `0.4.29`
  reports absent.
- **The real check is this ADR's own merge**: it must cut `0.4.30`, push
  `v0.4.30`, publish to crates.io, and create the GitHub release — the
  first green release run since ADR-0146. Anything less means the fix is
  wrong, and no further work should be stacked on top until it is green.

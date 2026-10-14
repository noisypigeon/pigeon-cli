# ADR-0144: survive a destination-endpoint outage, and explain a case-colliding source key

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Accepted.

## Context

A real `job run transform --input-file-type jpg` run over 38,232 source keys
pulled and pushed 37,955 files cleanly and then exited 1, reporting:

```
Pulled 37948 file(s); 0 transcoded, 37955 copied through, 277 failed; pushed 37955 file(s).
Skipped 120 already done, 0 AppleDouble sidecar(s).
Failures by stage: 2 pull, 0 transcode, 0 place, 275 push.
```

Reading the run's artifacts and `pigeon.jsonl` found **two unrelated
causes**. The counts reconcile exactly either way — `files_to_pull` 38232 =
37948 rclone-transferred + 282 `queued_from_local_scan` + 2 undelivered, and
`copied_through` 37955 = 38230 dispatched − 275 push failures — so nothing
else is unexplained.

### 1. A 74-second DNS blackout became 275 permanent per-file failures

All 275 push failures carry exactly one error and nothing else — no 4xx/5xx,
no `AccessDenied`, no `SlowDown`, no timeout:

```
275 / 275 push logs contain "no such host"
872  lookup s3.fr-par.scw.cloud: no such host
```

They fall inside **one continuous 74-second window** (02:55:48 → 02:57:02
local), and pushes resumed normally straight afterwards — 7,237 push logs
were written in the following nine minutes.

The macOS unified log names the fault: at `02:55:48.346`, `mDNSResponder`
returned `rdata: <none>` — a *negative* answer — for **both A and AAAA** of
`s3.fr-par.scw.cloud`, to a burst of concurrent `rclone` queries. Go's
resolver reports a negative answer as `no such host`, which is verbatim what
rclone logged. Every DNS question in the window resolved over
`interface: utun6/24, interface properties: {ipv4, vpn}` — a ProtonVPN
WireGuard tunnel that is this host's *only* primary resolver
(`scutil --dns` resolver #1 is `10.2.0.1` on `utun6`), so there was no
non-VPN path to fall back to.

Contributing environmental factor: the machine slept five times during the
run (3,102s total, per `pmset -g log`), which is exactly what destabilises a
WireGuard tunnel and its resolver. Pigeon's own numbers corroborate this
independently — `command finished` reported `elapsed_ms=15489289` (4h18m09s;
`Instant` is monotonic and does not advance during macOS sleep) against a
5h09m23s wall-clock span, a 3,072s gap matching the logged sleep to within
30 seconds.

**The amplifier is ours.** `CopyTo::retry()` returned `Some((3, 2s))`, and
`core::retry::retry_with_backoff` is linear, so attempts landed at
t=0/+2s/+6s — a **six-second budget against a 74-second outage**, confirmed
per-file in the logs (`02:55:48.359`, `02:55:50.450`, `02:55:54.538`; every
failed log holds exactly three rclone invocations). Worse, a negative DNS
answer is *negative-TTL cached*, so retrying within seconds is structurally
guaranteed to reproduce it: faster retries cannot help, only waiting past
the TTL can.

A correction worth recording, because it shaped this decision: the first
analysis claimed 275 files would each burn the full budget, making a longer
budget *worse*. That was wrong — the push semaphore already caps concurrent
pushers at `--upload-concurrency` (default 16), so at most 16 files are ever
retrying at once, and once the outage clears the remaining files sail
through. **A longer budget alone would have fixed this particular
incident.** The budget is therefore the primary fix, and the gate below
exists for the case the budget does *not* cover: an outage that outlasts it,
where every successive batch of `upload_concurrency` files re-pays the whole
budget before failing.

### 2. Two source keys differing only in case silently never arrived

The 2 pull failures were reported as:

```
IMG_0260.jpg  failed  pull  file was in this run's pull set but rclone never reported it as copied…
IMG_1551.jpg  failed  pull  …
```

The pull set contains **both case variants** of each, and these are the only
two case-insensitive duplicates among all 38,232 entries:

```
pull-files-from.txt:36891:IMG_0260.JPG     36945:IMG_1551.JPG
pull-files-from.txt:36892:IMG_0260.jpg     36946:IMG_1551.jpg
```

rclone's own pull log states the cause outright:

```
02:52:45.533 notice | IMG_0260.jpg | Duplicate object found in source - ignoring
03:07:56.312 info   | IMG_0260.JPG | Copied (new)
```

S3 keys are case-sensitive; the local staging tree is on case-insensitive
APFS. Both keys map to one local path, so rclone declines to clobber and
logs it at **`notice` level — not as an error**. That is precisely why
`pull_summary.errors == 0` while two files were genuinely undelivered, and
why ADR-0130's cross-check fired:

```
transform: rclone's pull error count disagrees with the number of undelivered files
  rclone_errors=0, undelivered_files=2
```

**ADR-0130's reconciliation worked correctly** — these are real gaps, and
the uppercase variants were transformed and pushed. Two secondary points
matter:

- `placement::compute_destination_name` hashes
  `{source_path}\x01{original_relative_path}` **case-sensitively**, so the
  two variants map to *distinct* destination names. There is no destination
  collision; the lowercase objects are simply missing. A rerun will not fix
  them, because the collision is deterministic.
- The reported detail is **false** for this class. rclone *did* report the
  file; the job just never reads `notice`-level lines. During the original
  investigation that wording actively misled — push logs existed for the
  colliding names (belonging to the uppercase variants), so the failures
  first looked like reconciliation false positives before the rclone log
  settled it.

## Decision

### 1. Classify an endpoint-level rclone failure

New `core::rclone::is_endpoint_unreachable_error`, matching `no such host`,
`dial tcp`, `connection refused`, `connection reset by peer`, `i/o timeout`,
`no route to host`, `network is unreachable`, `TLS handshake timeout`.

It lives in `core::rclone`, not in `transform::media`, because it is a
question about *rclone's* output — the same module that already
substring-matches rclone stderr for `Lsjson`'s `"directory not found"`.
`media.rs` is about `ffmpeg`.

It is deliberately a separate predicate from
`is_non_retryable_transcode_error` / `is_wrong_input_kind_error`, following
ADR-0143's precedent that separate questions get separate predicates. These
errors *are* retryable; what distinguishes them is that they are **shared** —
every concurrent transfer against the same remote fails identically — which
is what licenses handling them once per run rather than once per file.
Unlike ADR-0143's two guard consts, these match rclone/Go's own text rather
than pigeon's, so they are genuinely version-fragile; the doc comment says
so and cites issue #63's precedent.

### 2. Give `CopyTo` a budget sized to a real outage

`CopyTo::retry()` becomes `Some((4, Duration::from_secs(30)))` —
t=0/+30s/+90s/+180s, about five minutes, against the six seconds it
replaced.

`Lsjson`'s own `(3, 2s)` is **deliberately left alone**: it runs once per
run rather than once per file, and ADR-0140 specifically wanted an
unresolvable *source* to fail the run fast rather than pull zero files and
exit 0.

### 3. Wait out an outage once per run, not once per file

A new `EndpointGate` in `transform::worker`, threaded alongside the existing
`push_semaphore`. On an endpoint-level push failure, a task tries to win an
election (`tokio::sync::Mutex::try_lock`) and become *the prober*: it closes
the gate, sleeps a linearly-scaled backoff round, then reopens it. Every
other task failing the same way parks on a `watch` channel instead of
spawning its own doomed `rclone` subprocess, and retries only once the gate
reopens.

There is deliberately **no separate probe command** — the only listing
primitive available (`Lsjson` on `--destination-path`) is a full-tree
listing of a 38k-object prefix, far too expensive for a health check. The
real "is it back?" test is the next actual push.

After `PUSH_ENDPOINT_RECOVERY_ROUNDS` (4) fruitless rounds the gate
**latches**: `rounds` is never reset, so every remaining file fails
immediately rather than re-paying the recovery budget. That re-payment is
the specific amplifier an outage longer than §2's budget would otherwise
cause.

This is the push-leg analogue of ADR-0121/0143's transcode circuit breaker,
with one deliberate difference: it **pauses and resumes** rather than
aborting the run. A transient network fault is not the systemic
ffmpeg-capability gap that breaker exists for, so the response is patience,
not abandonment.

A push failure continues to **retain** both local copies — they are the only
resume state this job has. ADR-0124's post-push GC and ADR-0143's
wrong-input-kind deletion both sit on paths a push failure returns before
reaching, and a test pins this.

### 4. Detect case-colliding source keys, and say so honestly

`files_needing_pull` now computes `case_insensitive_collisions` over its own
`to_pull` output, carried on `PullSelection` (which exists precisely to
surface what ADR-0130 found being discarded silently). Every member of every
colliding group is recorded, not just the losers: which member rclone keeps
is its own ordering decision, not something this job can predict.

Both keys are still *asked for* — rclone decides what it can deliver — and
the collision is `tracing::warn!`ed before the pull begins, one line per
group with every key named. Naming the keys is the whole remedy; there is
nothing to fix automatically.

The reconciliation detail moves into a pure `undelivered_detail` function
(same extraction rationale as ADR-0143's `circuit_breaker_should_trip`: the
branch is the interesting part and is untestable buried in
`run_transform_job`'s loop) with an accurate message for the collision case,
and a narrower generic one for a genuine no-show.

## Out of scope

- **Actually pulling both case variants.** It needs distinct local paths (a
  per-file subdirectory, or case-disambiguating local names), which touches
  the local-tree layout that both ADR-0124's GC and the initial `source/`
  scan depend on — too much blast radius for 2 files in 38,232, when
  detect-and-report already removes the mystery. Filed per ADR-0031. ([#83](https://github.com/noisypigeon/pigeon-cli/issues/83))
- **Reading rclone's `notice`-level lines** into `RcloneLogSummary`. The
  up-front collision set answers the same question without new log-parsing
  surface, and is known before the pull even starts.
- **A non-VPN DNS fallback, or anything about host networking.** Pigeon does
  not configure the resolver, and should not.
- **Generalising the gate to `deduplicate`'s push leg.** One consumer so
  far; per this repo's duplicate-until-the-third-consumer precedent, it
  stays in `transform` until a second real need appears.
- Changing `Lsjson`'s retry budget, the transcode retry policy, or
  `CIRCUIT_BREAKER_SAMPLE_SIZE`.

## Consequences

- **A push leg now tolerates a multi-minute destination outage.** The
  74-second blackout that produced 275 failures would now be absorbed by §2
  alone, with the run exiting 0.
- **A long outage is bounded rather than amplified.** Once the endpoint is
  declared down, remaining files fail immediately instead of each re-paying
  a five-minute budget — but note this also means a run can now spend up to
  ~5 minutes of recovery backoff *per round* before giving up, so a genuinely
  dead destination takes longer to fail than it used to. That is the
  intended trade: a dead destination is rare, a blip is not.
- **An endpoint-level failure now parks every concurrent pusher.** If
  `is_endpoint_unreachable_error` ever matched a per-file fault by mistake,
  it would stall the push leg behind a wait that cannot help — which is why
  the classifier is tested against 403/503/per-file-read errors explicitly.
- **A case-colliding source key is now explicable** before the pull starts
  and accurate in the report. It is still *not delivered*: this ADR buys
  diagnosis, not the file.
- The generic undelivered-file message is now narrower, and no longer claims
  something false about a whole class of failure.
- `tokio`'s `test-util` feature is added as a **dev-dependency only**, for
  `start_paused`'s virtual clock. Nothing in the shipped binary depends on a
  pausable clock.

## Amends

ADR-0121 (adds a pause-and-resume push analogue to its abort-only transcode
breaker), ADR-0130 (replaces the undelivered-file message it introduced for
one class, and adds a count to its pull-phase line), ADR-0132 (changes
`CopyTo`'s retry budget, which it consolidated), and ADR-0143 (extends its
separate-predicates-for-separate-questions reasoning to a third predicate).
Each gets a short forward-pointing "Amendment" section, per this repo's
convention for amending a merged ADR without rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `core::rclone`: `is_endpoint_unreachable_error` matches the **verbatim**
  275-file error from the motivating run, plus each other connectivity
  signature; and rejects `AccessDenied` (403), `SlowDown` (503), a per-file
  `no such file or directory`, and `Lsjson`'s `directory not found`.
- `worker.rs`: 16 tasks failing on one shared outage elect **one** prober
  and burn **one** round (`rounds == 1`) with all 16 told to retry — the
  property that distinguishes this from 275 independent waits. The test
  passing at `rounds == 1` also proves the tasks genuinely overlapped.
- `worker.rs`: the gate returns `Retry` for each of the 4 rounds, then
  `GiveUp`, and **stays** latched for later callers; a task arriving at an
  already-latched gate takes the waiter branch and also gives up rather than
  parking.
- `worker.rs`: `case_insensitive_collisions` finds exactly the real
  `IMG_0260.JPG`/`IMG_0260.jpg` + `IMG_1551` pairs and leaves a twin-less
  key alone; covers stem collisions; and treats the same basename in
  different directories as distinct.
- `worker.rs`: `files_needing_pull` still returns **both** colliding keys in
  `to_pull` while reporting them in `case_collisions`; and
  `compute_destination_name` is asserted to map the two variants to distinct
  destination names, pinning the "no destination collision" claim.
- `worker.rs`: `undelivered_detail` returns the collision explanation for a
  colliding key and the generic one otherwise, with the collision arm
  asserted *not* to contain "never reported it as copied".
- The pre-existing ADR-0143 retention test stays green **unmodified**.
- Manual: rerun the motivating job against its existing `--local-output` to
  push the 275 retained files (all 275 `source/` and `result/` copies are
  intact, so no re-pull), under `caffeinate -is`.

## Amendment (2026-10-14): both case variants are now pulled (ADR-0152)

This ADR deferred the fix to [#83](https://github.com/noisypigeon/pigeon-cli/issues/83),
reasoning that pulling both members of a colliding pair needed *"the
local-path layout changes ADR-0124's GC and the initial `source/` scan both
depend on."* ADR-0152 finds that the layout change is not needed, because
`PendingFile` already separated identity (`relative_path`, this ADR's
reconciliation key and `group_key`'s hash input) from location
(`absolute_path`) -- nothing derived one from the other, so a colliding key
can be pulled somewhere else and keep its key. The coupling lived in two
lines.

Two things this ADR introduced are therefore replaced. Its pre-pull warning
said *"rclone will deliver at most one of them"*, which stops being true: it
becomes an INFO stating that each key is pulled individually to its own path.
And `undelivered_detail`'s collision arm -- correctly narrower than the
generic one, which this ADR rightly called a false statement for this class --
now describes a **failed per-key pull** and carries rclone's actual error,
since that is the only way a colliding key can go undelivered.

The detection itself (`case_insensitive_collisions`, every member of every
colliding group, named in the log before the pull) is what made the fix
cheap, and is unchanged. So is this ADR's verified observation that
`compute_destination_name` hashes case-sensitively, so the variants never
collide at the destination -- a property it proved and ADR-0152 finally
exercises.

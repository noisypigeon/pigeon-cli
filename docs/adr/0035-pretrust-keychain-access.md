# ADR-0035: pre-trust the running binary at keychain-write time

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Rejected -- see the amendment below. The `-T` pre-trust
  hypothesis was disproved by a live implementation attempt; no code
  from this ADR is in `main`.

## Context

A real `pigeon job run email-sync` run triggers the macOS "pigeon
wants to access key 'pigeon' in your keychain" prompt 5-10 times in a
single invocation, one dialog at a time, each requiring the user's
login-keychain password before it proceeds.

### Why it happens 5-10 times, not once

`service/pigeon-cli/src/core/keyring/credentials.rs`'s `get_secret`/`set_secret`/
`delete_secret` each open a `keyring::Entry::new("pigeon", alias)` (the
`keyring` crate, `v4.2.0`, pinned in `Cargo.toml`; its macOS backend is
`apple-native-keyring-store`). Reading that vendored crate's
`Cred::build`/`set_generic_password` confirms every distinct `alias`
becomes a genuinely distinct macOS Keychain "generic password" item,
uniquely identified by its `(service="pigeon", account=alias)` pair.

`service/pigeon-cli/src/commands/job/email_sync/wizard.rs::dispatch_async` calls
`credentials::get_secret` once per selected identity (a loop, line
~441), once more for the target bucket-config if uploading (~498), and
once more for the encryption key if encrypting (~526). A fourth call
site with the identical shape exists in
`service/pigeon-cli/src/commands/job/decrypt_files/wizard.rs:219`. For N selected
identities plus upload/encryption, that's `N + 2` *distinct* keychain
items touched in a single run -- this fully explains "5-10 times" on
its own; no call site fetches the same alias twice.

macOS Keychain access control is granted **per item**, not per
application: a grant (even "Always Allow") for one item never covers
another. ADR-0016 already fixed a *different*, adjacent problem -- an
unstable ad-hoc code signature invalidating "Always Allow" grants
across every `cargo build` rebuild (`.mise.toml`'s `codesign --force
-s - --identifier "dev.pigeon.cli"` step). That fix makes a grant
*survive rebuilds*; it does nothing about needing `N + 2` *separate*
grants for `N + 2` distinct items in the first place, which is the gap
this ADR closes.

### Why even `pigeon`'s own later reads still prompt

Run directly on this machine, `security add-generic-password -h`
states: *"By default, the application which creates an item is
trusted to access its data without warning. ... -T  Specify an
application which may access this item (multiple -T options are
allowed)."* The `keyring` crate's public `Entry` API (`v1.rs`) exposes
only `new`/`set_password`/`get_password`/`delete_credential` -- no way
to set a trusted-application ACL at creation time. Items created via
the modern `SecItemAdd`-family API (what `apple-native-keyring-store`
uses under the hood) most likely don't reliably inherit "creator is
trusted" the way the legacy `security`-CLI-documented default implies,
absent an explicit `-T` grant -- this is the most likely explanation
for why even `pigeon`'s own subsequent reads of an item it created
still prompt, not something verifiable live in this environment (no
real interactive macOS Keychain UI available to an agent).

### Direction considered and chosen

A design-review pass also evaluated consolidating every secret into
one (or a few, partitioned-by-kind) keychain items, which would cut
prompts to 1-3 regardless of how many aliases exist, at the cost of a
larger blast radius (one grant exposes every secret at once) and a
worse Keychain Access UI experience (one opaque blob instead of
per-identity items). The chosen direction instead **pre-authorizes the
running `pigeon` binary as a trusted application at the moment a
secret is written**, via the `security` command-line tool's `-T` flag
-- keeping today's one-item-per-alias model, unchanged blast radius,
and unchanged Keychain Access visibility.

## Decision

On macOS only (`#[cfg(target_os = "macos")]`), `set_secret`/
`get_secret`/`delete_secret` in `service/pigeon-cli/src/core/keyring/credentials.rs` shell
out to `/usr/bin/security` instead of the `keyring` crate, using the
exact flag syntax confirmed via `security <subcommand> -h` on this
machine:

- **`set_secret`**:
  ```
  security add-generic-password -a <alias> -s pigeon -w <secret> -T <path> -U
  ```
  where `<path>` is `std::env::current_exe()` at call time (the actual
  running binary, whatever its install location) and `-U` makes this
  idempotent (create-or-update), matching today's crate-based
  behavior. This is the one call that grants the trust -- every
  `pigeon keyring add`/`modify` from now on pre-authorizes the running
  binary for that alias's item, so `job run email-sync` never has to
  prompt for it again.
- **`get_secret`**:
  ```
  security find-generic-password -a <alias> -s pigeon -w
  ```
  (prints only the password to stdout). Using the same `security` tool
  family for both read and write -- rather than mixing the legacy
  `security`-CLI write path with the `keyring` crate's modern-API read
  path -- avoids depending on cross-API ACL interoperability this
  investigation can't verify live.
- **`delete_secret`**:
  ```
  security delete-generic-password -a <alias> -s pigeon
  ```
  mapping the not-found case to `Ok(())`, matching today's
  `keyring::Error::NoEntry`-is-success semantics. The exact detection
  mechanism (exit status vs. stderr text) needs live confirmation
  during implementation -- not guessable from this environment.
- Non-macOS platforms keep the existing `keyring`-crate-based
  implementation completely unchanged, `cfg`-gated -- this problem is
  macOS-specific (Linux Secret Service / Windows Credential Manager
  aren't evidenced as broken, mirroring ADR-0016's own scoping).

### Existing, already-configured aliases

No automatic migration. Each already-configured identity,
bucket-config, or encryption key needs one `pigeon keyring modify
<kind> <alias>` (or a delete-and-re-add) to pick up the `-T` grant,
since `set_secret` is the only path that grants it. This mirrors
ADR-0016's own precedent exactly ("one more manual re-add should be
expected immediately after adopting it, then it should stop
recurring") rather than adding an automatic-migration mechanism (e.g.
having `get_secret` opportunistically re-write every item it reads),
which would multiply subprocess calls and argv-exposure risk (below)
on every *read* instead of only at write time.

## Consequences

- **Secret exposed briefly via process argv.** `security`'s own help
  text warns: *"Use of the -p or -w options is insecure... Specify -w
  as the last option to be prompted."* Passing `-w <secret>` means the
  plaintext secret is visible, briefly, via `ps`, to any other process
  running as the same local user or root during that one `security`
  invocation. This is a real, new exposure window that doesn't exist
  today -- the `keyring` crate passes secrets via a Security.framework
  API call, never through a spawned process's argv. For a single-user
  local CLI whose threat model already stages plaintext message
  content to disk (`email-sync`'s whole design), this is judged an
  acceptable, narrow, local-only tradeoff, but it is a real behavior
  change worth naming plainly rather than glossing over.
- **Needs live verification.** Whether `-T`-granted trust actually
  suppresses the prompt for this specific crate/OS-version combination
  cannot be confirmed in this environment (no real macOS Keychain
  interaction available to an agent). Concrete verification step for
  whoever implements this: `keyring add` (or `modify`) an identity,
  then run `job run email-sync` twice in a row and confirm the second
  run doesn't prompt for that identity's secret.
- Ties the trust grant to the exact running binary's path/identity at
  write time; combined with ADR-0016's stable ad-hoc codesign
  identifier, this should remain valid across rebuilds too (the same
  reasoning ADR-0016 already established for "Always Allow" surviving
  rebuilds) -- also unverified live, same caveat.
- `core/keyring/credentials.rs` gains its first `std::process::Command`
  usage and its first `#[cfg(target_os = ...)]` platform split -- no
  other application code in this repo shells out today (only
  `.mise.toml`'s build tooling does, via `codesign`).

## Out of scope

- Consolidating multiple aliases into fewer keychain items (considered
  during design review as both a full-consolidation and a
  partitioned-by-kind alternative; rejected in favor of pre-trust,
  which keeps today's per-alias isolation and Keychain Access
  visibility unchanged).
- Automatic migration of already-existing, pre-fix keychain items --
  deferred in favor of the one-time manual `keyring modify` step,
  matching ADR-0016's precedent.
- Real code-signing (a paid Apple Developer ID certificate,
  notarization) for a fully trusted, zero-shell-out ACL grant -- the
  same boundary ADR-0016 already drew for this project.
- Linux Secret Service / Windows Credential Manager prompting
  behavior -- not evidenced as broken.

Implementation is a separate, later task.

## Amendment (2026-09-25): the `-T` hypothesis was disproved live -- rejected

### Context

This ADR's Consequences section named its central open question
plainly: *"Whether `-T`-granted trust actually suppresses the prompt
for this specific crate/OS-version combination cannot be confirmed in
this environment... Concrete verification step for whoever implements
this: `keyring add` (or `modify`) an identity, then run `job run
email-sync` twice in a row and confirm the second run doesn't prompt."*

Implementation was attempted on this same machine (a real macOS dev
environment) exactly per the Decision above:
`set_secret`/`get_secret`/`delete_secret` were rewritten to shell out
to `/usr/bin/security`, `-T <current-exe-path>` on write. Before
opening a PR, a live smoke test round-tripped a disposable alias
(`__pigeon_adr_0035_smoke_test__`) through the real Keychain: delete
if present, `set_secret`, `get_secret` (first read), `get_secret`
again (the read that should *not* prompt if `-T` worked), then
`delete_secret`. The test's own assertions all passed -- the
round-tripped value and cleanup were correct -- but it took ~27
seconds to run, unusually long for a handful of no-UI subprocess
calls. Asked directly, the person running it confirmed a Keychain
access-control dialog appeared **more than once** during that single
test run.

### Finding: `-T` naming `pigeon`'s path doesn't cover `security`'s own requests

The test passing functionally while still prompting repeatedly means
the `-T` pre-trust grant did not suppress prompts as hypothesized --
this ADR's core bet is wrong, not merely unverified. The most likely
explanation, flagged as a risk during planning before implementation
even started: once `get_secret`/`delete_secret` shell out to
`/usr/bin/security`, the process actually asking the Keychain for
access is **`/usr/bin/security`**, not `pigeon`. `-T <path-to-pigeon>`
only names *pigeon's* binary path as trusted -- it says nothing about
`/usr/bin/security` itself, which is the process making every read/
delete request in this design. Today's baseline prompt (before any of
this ADR's changes) names "pigeon" as the requester specifically
*because* `pigeon` calls the `keyring` crate's Security.framework
bindings directly, with no intermediary process. Routing through
`security` instead replaces one requesting identity (`pigeon`, ad-hoc
signed, rebuildable) with a different one (`/usr/bin/security`, a
stable Apple-signed system binary) that this ADR's `-T` grant never
authorizes.

This explanation is judged most likely, not conclusively isolated --
no further live experiments were run to confirm it precisely (e.g.
retrying with `-T` naming `/usr/bin/security`'s own path instead of or
in addition to `pigeon`'s, or dropping `-T` entirely to test whether
`security`'s own documented "creator is trusted" default already
covers its own later reads independent of `-T`). Once the core
hypothesis failed, further iteration was set aside pending direction
on whether pre-trust is worth pursuing further at all, rather than
guessing again live against a real Keychain.

### Decision

Reject this ADR's approach. The implementation attempt (a working
tree change to `service/pigeon-cli/src/core/keyring/credentials.rs`) was discarded before
being committed -- no code from this ADR ever landed in `main`, and
`credentials.rs` is unchanged from its pre-ADR-0035 state. Status
above changed from Accepted to **Rejected**.

### Consequences

- The original problem -- `job run email-sync` prompting for the
  macOS keychain password 5-10 times per run -- remains unsolved.
  This ADR's Context/investigation section (why it happens 5-10 times,
  not once) is still accurate and reusable by a future attempt; only
  the Decision (the `-T`-via-`security`-CLI fix) is disproved.
- A future attempt at this problem should either: (a) revisit the
  consolidation alternative this ADR's own design-review pass already
  evaluated and set aside in favor of pre-trust (full single-item or
  partitioned-by-kind, per the "Direction considered and chosen"
  section above), since it doesn't depend on guessing which process's
  code identity macOS's Keychain ACL actually checks; or (b) run
  narrower, isolated live experiments (varying one variable at a time
  -- which path `-T` names, whether `-T` is needed at all) before
  committing to a design, rather than implementing the full three-
  function change and finding out via one combined smoke test.
- The live-smoke-test methodology itself worked as intended: it caught
  a wrong hypothesis before it shipped, exactly what it was added for.
  Worth keeping as the verification pattern for any future keychain-
  behavior ADR in this repo, even though this specific attempt failed.

### Out of scope (this amendment)

- Isolating the exact mechanism further (testing `-T` variants,
  testing no-`-T` at all) -- not pursued; left to whichever future
  attempt picks this problem back up.
- Implementing the consolidation alternative -- a new decision, not
  this amendment's to make.

No code changes accompany this amendment -- it is a documentation-only
correction to the record.

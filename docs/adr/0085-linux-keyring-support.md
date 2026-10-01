# ADR-0085: Linux keyring support via kernel keyutils

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

`pigeon keyring add` fails outright on a headless Ubuntu (Scaleway) VM:

```
Error: failed to open keychain entry for 'test': No default store has been set, so cannot search or create entries
```

ADR-0003 named this exact risk when the `keyring` crate was first adopted, as an accepted-but-unaddressed risk rather than something built against: *"`keyring` on Linux depends on a running Secret Service daemon (e.g. `gnome-keyring`, KWallet); headless/server Linux environments without one will need a documented fallback or explicit error rather than a silent failure."* It's now materializing for real on real hardware.

### Root cause

- `service/pigeon-cli/Cargo.toml` depends on `keyring = "4.2.0"` with no feature flags, so it resolves to the crate's *default* features: `v1` (the stable `Entry` API), `windows-native-keyring-store`, and — on Linux — `zbus-secret-service-keyring-store`, a pure-Rust D-Bus Secret Service backend.
- That backend needs an active D-Bus session plus a running Secret Service daemon (`gnome-keyring`, KWallet). That's standard on a Linux *desktop* session; it's simply absent on a headless server VM like this Scaleway instance, which has no desktop environment, no login session bus, and no Secret Service provider installed.
- When no compiled-in backend can initialize, `keyring-core` never establishes a default credential store — which is exactly what the generic "No default store has been set" message is reporting. It isn't a bug in `pigeon`'s own code; it's `keyring` surfacing that it has nothing to talk to.
- Every use of the crate in this codebase is already centralized in one place per ADR-0022's unified keyring command: `service/pigeon-cli/src/core/keyring/credentials.rs`'s `set_secret`/`get_secret`/`delete_secret`, each a plain `keyring::Entry::new(SERVICE_NAME, alias)` call. None of them assume a specific backend — the fix doesn't need to touch this file.
- Per ADR-0022 §4, `add` calls `credentials::set_secret` *before* writing the `keyring.toml` metadata entry, specifically so a keychain failure rolls back cleanly. The failed `add` in the bug report therefore never persisted a dangling `keyring.toml` entry — there's nothing to clean up as part of this fix.

### Fix direction

The `keyring` crate also ships `linux-keyutils-keyring-store`: a credential store backed directly by the Linux kernel's keyutils facility, needing no daemon, no D-Bus session, and no desktop environment — it works immediately, including as root. Its own maintainers recommend it explicitly for this situation: *"If you are trying to use the keyring crate on a headless linux box, or one that doesn't come with gnome-keyring, it's strongly recommended that you use this credential store... it's always available on Linux."*

The trade-off, confirmed against upstream docs before committing to this direction: kernel keyring key material is **in-memory only**. It does not survive a reboot, and a given key also expires after a period of disuse (the kernel's `persistent_keyring_expiry`, refreshed on each access) — so after a restart, every Linux-stored secret needs re-adding via `pigeon keyring add`/`modify`.

**Confirmed with the user**: this Scaleway box is run interactively and ad hoc — SSH in, run `pigeon` by hand — not as an unattended cron job. Reboot-persistence is therefore not a requirement right now, and the kernel-keyutils fix is sufficient. A reboot-durable alternative (e.g. a Secret Service daemon unlocked automatically at boot) was considered and explicitly set aside: it's a materially bigger lift for a need that doesn't exist yet on this deployment.

## Decision

### Linux-only `keyring` feature override

`service/pigeon-cli/Cargo.toml` keeps its existing base dependency line unchanged — this continues to cover macOS (`apple-native-keyring-store`) and Windows (`windows-native-keyring-store`) exactly as today:

```toml
[dependencies]
keyring = "4.2.0"
```

A new Linux-target-specific override drops the D-Bus Secret Service backend and enables the kernel-keyutils one instead:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
keyring = { version = "4.2.0", default-features = false, features = ["v1", "linux-keyutils-keyring-store"] }
```

This is a deliberate, narrowly-scoped choice, not an attempt to support every possible Linux configuration: this project's only evidenced Linux usage is a headless server VM, not a Linux desktop running `gnome-keyring`/KWallet. `core/keyring/credentials.rs` needs no code changes — its `Entry`-based calls are already backend-agnostic, which is exactly what makes a Cargo.toml-only fix possible.

### No error-message changes

`credentials.rs`'s existing generic `format!("failed to ... : {err}")` wrapping (from ADR-0022) stays as-is. The root cause is eliminated outright by the dependency change rather than papered over with a better error message for a failure mode that should no longer occur in the evidenced use case — consistent with this project's standing preference against speculative error-handling for scenarios that can't happen.

### Reboot caveat, documented

`service/pigeon-cli/README.md` gains a short note under its keyring/setup section (or this ADR's Consequences section stands as the record, if README doesn't cover keyring setup in enough detail to warrant one) stating that Linux secrets live in the kernel keyring and need re-adding via `pigeon keyring add`/`modify` after a reboot.

## Consequences

- `pigeon keyring add/modify/delete/list`, and every job's secret lookups (`get_secret`), work immediately on headless Linux — no daemon, no D-Bus session, no desktop environment required.
- Secrets do not survive a reboot on Linux (kernel key material is in-memory-only) and expire after a period of disuse. This is an accepted, named trade-off given the confirmed ad-hoc/interactive usage pattern on this box, not unattended automation — if that changes, this decision needs revisiting (see Out of scope).
- Linux Secret Service (`gnome-keyring`/KWallet) support is dropped entirely rather than kept alongside kernel-keyutils — there's no evidenced need for it today. A future Linux desktop deployment target would need this decision reopened, not silently worked around.
- `keyring.toml` metadata (aliases, non-secret fields) is completely unaffected by this change and keeps persisting normally on disk across reboots — only the OS-level secret itself needs re-entry after a restart.
- macOS and Windows builds, and every existing macOS-specific keychain decision (ADR-0016's stable codesign identifier, ADR-0035's rejected pre-trust attempt), are entirely unaffected — the base `[dependencies]` line is untouched.

## Out of scope

- Reboot-persistent / unattended-automation-safe secret storage on headless Linux (e.g. a Secret Service daemon unlocked automatically at boot, or a custom encrypted-file-backed credential store) — not needed under the confirmed ad-hoc usage model on this box today. ([#1](https://github.com/noisypigeon/pigeon-cli/issues/1))
- Linux desktop (D-Bus Secret Service / `gnome-keyring` / KWallet) support — not evidenced as a real `pigeon` deployment target.
- Any change to `core/keyring/credentials.rs`'s error-handling or messages — this fix addresses the root cause directly rather than improving the symptom's error text.
- Recovering or migrating any already-attempted keyring entries from before this fix — confirmed none exist, since the failed `add` in the bug report never reached the metadata-write step (ADR-0022 §4's set-secret-then-persist ordering).

Implementation is a separate, later task.

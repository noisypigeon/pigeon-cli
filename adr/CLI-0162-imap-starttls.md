# CLI-0162: IMAP STARTTLS, so Proton Mail Bridge works in its default mode

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-20.
- **Status**: Accepted. Implemented on 2026-10-20, and checked against a
  live Bridge (see Verification). Amends CLI-0003.

## Context

The first Proton account set up with `pigeon keyring add email` failed
before any credential was checked:

```
✔ Select a provider · proton
App password for <address>: [hidden]
Error: TLS handshake with 127.0.0.1:1143 failed: record overflow
```

CLI-0003 decided that "TLS is implicit on port 993", with Proton the
exception only because Bridge's certificate is self-signed.
`imap_client::connect_and_login` implemented exactly that for every
provider: open a TCP socket, then start a TLS handshake immediately.

Bridge does not speak implicit TLS on its default port. A probe of Bridge
03.27.01 on 127.0.0.1:1143 showed all three facts:

- **It greets in plaintext and advertises STARTTLS.** The first bytes are
  `* OK [CAPABILITY AUTH=PLAIN ID IDLE IMAP4rev1 MOVE STARTTLS UIDPLUS UNSELECT] Proton Mail Bridge 03.27.01`.
- **A direct TLS handshake fails.** `openssl s_client -connect 127.0.0.1:1143`
  reports `wrong version number`.
- **STARTTLS succeeds.** `openssl s_client -starttls imap` negotiates TLS 1.3
  with Bridge's self-signed certificate.

"Record overflow" is how macOS's TLS stack reports reading `* OK [CAPA…` as
a TLS record header; OpenSSL calls the same thing "wrong version number".
So pigeon has **never** been able to reach Bridge in its default mode. That
covers both callers of `connect_and_login`: credential verification in
`keyring add`/`modify email` (through `verify_login`), and every connection
`job run email import` opens (CLI-0146). CLI-0003's own risk list names
"Proton Bridge's self-signed local certificate", but the transport mode was
the real gap.

Bridge can also be switched to SSL (implicit TLS) in its settings, and its
port moves if 1143 is taken. So no single fixed answer is right for every
Bridge.

## Decision

### 1. Two security modes, and no plaintext one

A new `ImapSecurity` enum has two values: `Tls` (implicit, from the first
byte) and `StartTls` (plaintext greeting, then `STARTTLS` before anything
else). There is deliberately **no plaintext mode**. `LOGIN` never goes over
an unencrypted socket, not even to a Bridge on localhost.

### 2. Defaults come from the provider, overrides from the identity

`Provider::default_security(port)` decides when nothing else does:

| Provider | Default |
| --- | --- |
| Gmail, Fastmail, iCloud | `tls` (993) |
| Proton | `starttls` (Bridge's default on 1143) |
| Custom | `starttls` on port 143, `tls` otherwise (the IANA convention) |

`Identity` gains `#[serde(default)] security: Option<ImapSecurity>`, and
`Identity::security()` returns it, falling back to the provider default. It
is stored only when it **differs** from that default (`stored_security`).
A choice that matches the default stays `None`, so a later change to the
default, or to the port, still applies.

Existing `keyring.toml` entries need no migration. A missing field means the
provider default, which is exactly today's behaviour for every provider
except Proton, and Proton never worked.

### 3. STARTTLS in `connect_and_login`

For `StartTls`, `connect_and_login_inner` runs these steps:

1. Opens a plain TCP socket and wraps it in an `async_imap::Client`.
2. Reads the greeting.
3. **Refuses a greeting whose `[CAPABILITY …]` code omits `STARTTLS`.**
4. Sends `STARTTLS` and requires a tagged `OK`.
5. Takes the raw socket back with `into_inner()` and runs the same
   `TlsConnector` on it. That handshake keeps Proton's
   `accept_invalid_certs` as before.
6. Logs in.

Details:

- **No greeting is read after the upgrade.** RFC 3501 §6.2.1 sends none
  after STARTTLS. The implicit-TLS path still reads its greeting as before.
- **The downgrade guard is that nothing before the handshake sends a
  credential.** Every failure in steps 2 to 4 is an `Err`. A server that
  cannot or will not upgrade therefore gets no `LOGIN` at all, rather than
  one in the clear.
- **A greeting with no capability code is not evidence either way.** It
  falls through to the `STARTTLS` command itself, and a `BAD` or `NO` reply
  ends the attempt.
- `ImapSession` is unchanged (`Session<TlsStream<TcpStream>>`), because both
  modes end in the same TLS stream over the same socket. No caller's types
  change.
- `CONNECT_TIMEOUT` still bounds the whole sequence.

`connect_and_login` and `verify_login` take an `ImapSecurity`. The email
import worker passes `ctx.identity.security()`.

### 4. `--security` on `keyring add email`, a prompt on `modify`

- **`keyring add email --security tls|starttls`** overrides the default. Its
  main use is a Bridge switched to SSL mode. `plain` is rejected by clap.
- **`add` does not prompt for it.** A prompt there would break a piped
  secret, the non-TTY case CLI-0003's masked-input note describes and the
  CLI tests rely on.
- **`modify` (already fully interactive) gains a "Connection security"
  prompt**, defaulting to the identity's current effective mode.

### 5. A hint on the failure that started this

When an implicit-TLS handshake fails with `record overflow` or
`wrong version number`, the error now ends with:

```
-- the server sent plaintext, so it may expect STARTTLS; retry with --security starttls
```

A pure `tls_mode_hint` makes this testable.

### 6. `--host`/`--port` doc comments corrected

The flags' help said they were "ignored for known providers". That was
never true: `resolve_host_port` has always let explicit values win for every
provider. The help now says they default to the provider's values,
including Bridge's 127.0.0.1:1143, and that `--port` is how to reach a
Bridge listening elsewhere. Behaviour is unchanged.

## Consequences

- Proton Mail Bridge works with no flags, for both `keyring add email` and
  `job run email import`.
- A Bridge in SSL mode needs `--security tls` when added. One added before a
  switch needs `keyring modify` afterwards.
- Gmail, Fastmail, iCloud and custom servers on 993 behave exactly as
  before. Nothing new reaches their code path, which is today's
  implicit-TLS code untouched.
- A custom server on 143 now defaults to STARTTLS. Before this, it failed
  the same way Bridge did.

## Out of scope

- **A plaintext IMAP mode.** A permanent boundary, per §1.
- **Pinning Bridge's certificate instead of `accept_invalid_certs`.** Bridge
  regenerates its certificate, so pinning would need a re-pin flow. The
  connection never leaves localhost, so the trust gap is small. It is left
  as is, and not filed as an issue, unless that changes.

## Verification

- **Unit tests:**
  - `default_security` covers each provider and port.
  - `Identity::security()` falls back correctly and honours an override.
  - A pre-CLI-0162 Proton entry with no `security` field still loads and
    resolves to `starttls`.
  - `stored_security` stores only an override.
  - `tls_mode_hint` matches both plaintext signatures and nothing else.
  - `ImapSecurity` stores as `"tls"`/`"starttls"`.
- **Fake-server tests of the downgrade guard** run on 127.0.0.1 with no
  certificate. A greeting that advertises capabilities without `STARTTLS`,
  and a server answering `STARTTLS` with `BAD`, both produce an error. In
  both, the server **receives no `LOGIN` line and never sees the secret**.
- **CLI tests:** `--security` appears in `keyring add email --help`, `plain`
  is rejected, and a `starttls` attempt against a closed port fails without
  persisting an entry.
- **Live, against Proton Mail Bridge 03.27.01 on 127.0.0.1:1143**, with a
  throwaway `PIGEON_CONFIG_DIR` and a deliberately wrong password:
  - `--provider proton` now gets past STARTTLS and TLS and reaches `LOGIN`,
    which Bridge rejects: `login failed: … "no such user"`.
  - `--security tls` reproduces the original `record overflow`, now with the
    hint.
  - Nothing was persisted.
- **Not verified here:** a successful `LOGIN` with a real Bridge password,
  and a `job run email import` over it. Both need the account owner's real
  credential.

# ADR-0003: IMAP connectivity and authentication

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

ADR-0001 defines the `pigeon email` interface (`authenticate`, `list-identities`, `sink`, `transform`) across Gmail/Google Workspace, Fastmail, iCloud, and Proton, and calls for "support for application keys (or other mechanisms to avoid user/pass authentication)" without specifying how. ADR-0002 scaffolded the CLI around that interface, but every `email` subcommand is a stub — no IMAP crate, authentication mechanism, or credential storage has been chosen. This ADR makes those decisions before any real connectivity code is written.

## Decision

### IMAP crate and TLS

- Confirming ADR-0001's tentative choice: [`async-imap`](https://github.com/chatmail/async-imap) (chatmail-maintained, actively developed, RFC 3501, `tokio`-based). It's used in production by Delta Chat, and its `Client::login(user, pass)` method maps directly onto the app-password authentication chosen below — `Client::authenticate(mechanism, ...)` (SASL) stays available if XOAUTH2 is ever added later.
- TLS is implicit on port 993 (Gmail, Fastmail, iCloud) via `async-native-tls` wrapping a `tokio::net::TcpStream` before handing it to `async_imap::Client::new`. Proton (below) is the exception: its Bridge exposes a local IMAP endpoint with a self-signed certificate that `pigeon` will need to trust explicitly rather than validate against a public CA.

### Authentication mechanism: app/bridge passwords, not OAuth2

`pigeon` authenticates using each provider's app-specific (or bridge) password over plain IMAP `LOGIN`, not OAuth2/XOAUTH2, for v1:

- Gmail IMAP access via OAuth2 requires the `https://mail.google.com/` scope, which Google classifies as a **restricted scope**. Taking an OAuth client to "Production" with a restricted scope requires an annual, paid third-party **CASA security assessment** — infrastructure built for multi-tenant products, wildly disproportionate for a personal, single-user tool authenticating the author's own identities.
- Leaving the OAuth client in "Testing" status avoids CASA but caps the project at 100 registered test users and, more importantly, **forces refresh tokens to expire after 7 days**. That means weekly re-authentication for every identity — incompatible with "authenticate once, sink whenever."
- **Gmail App Passwords** (16-character credentials generated at `myaccount.google.com/apppasswords`, gated on 2-Step Verification being enabled) authenticate over IMAP `LOGIN` exactly like a normal password, require no Google Cloud project or OAuth client registration at all, and don't expire until revoked or the account password changes.
- **Fastmail** and **iCloud** use the identical shape natively: Fastmail issues app passwords from its account settings; iCloud requires an app-specific password from `appleid.apple.com` and has no third-party OAuth path for IMAP at all.
- **Proton** has no direct IMAP server (mail is end-to-end encrypted server-side). It requires **Proton Mail Bridge** running locally, which decrypts mail and re-serves it over local IMAP, issuing its own bridge-specific password. This is the same `LOGIN`-based shape, just pointed at `127.0.0.1` instead of a public host.
- OAuth2 (via the `oauth2` crate, PKCE, loopback redirect for the authorization step, refresh-token exchange thereafter) is explicitly deferred, not rejected outright — worth revisiting if a provider ever deprecates app passwords, or if `pigeon` ever needs to support many users/production-scale verification.

### Credential lifetime

| Provider | Credential | Typical lifetime |
|---|---|---|
| Gmail | App Password | Indefinite — until revoked or account password changes |
| Fastmail | App Password | Indefinite — until revoked |
| iCloud | App-specific Password | Indefinite — until revoked or Apple ID password changes |
| Proton | Bridge Password | Indefinite — until regenerated in Bridge |
| (Deferred) OAuth2 | Access token | ~1 hour |
| (Deferred) OAuth2 | Refresh token | Long-lived in production; forced to 7 days while the OAuth client is in Testing status |

All four v1 credentials are effectively long-lived, stored-once secrets with no rotation protocol — this is what makes simple at-rest storage (below) sufficient, instead of needing token-refresh machinery.

### Local credential storage

Storage is split between non-secret metadata and the secret itself:

- **Metadata** (alias, email, provider, IMAP host/port) lives in a local config file, e.g. `~/.config/pigeon/identities.toml`, with its path resolved via the `directories` crate so it lands in the correct OS-conventional location. `list-identities` reads only this file — it never touches the keychain, so listing identities can never surface a secret.
- **The secret** (app/bridge password) is stored in the OS-native secure credential store via the `keyring` crate (macOS Keychain, Linux Secret Service, Windows Credential Manager), keyed by alias. It is never written to disk in plaintext.
- `authenticate` prompts for the secret interactively and masked, via `dialoguer::Password`, rather than accepting it as a CLI argument — so it never lands in shell history or is visible via `ps`.

### Secret input: TTY vs. piped stdin

`dialoguer::Password` (the masked, interactive prompt used for entering the app/bridge password) requires a real terminal — it errors with "not a terminal" when stdin is piped, which is exactly the case for black-box `assert_cmd` tests of `authenticate`'s failure paths (per ADR-0002's testing approach) and for anyone scripting `pigeon` with a secret piped in from elsewhere (e.g. a password manager: `pass show gmail | pigeon email authenticate ...`).

`authenticate` therefore checks `std::io::stdin().is_terminal()` and picks the input method accordingly:
- **TTY**: `dialoguer::Password`, masked and interactive, as originally decided above.
- **Not a TTY** (piped/redirected stdin): read a single line from stdin directly and use it as the secret verbatim.

This is a deliberate, minimal branch rather than a workaround bolted on for tests: it makes `pigeon` scriptable with a piped secret as a side benefit, and it's what makes `authenticate`'s failure paths exercisable in `tests/cli.rs` without a real TTY. The secret is still never accepted as a CLI argument in either mode, so it never lands in shell history or `ps`.

### Multi-provider management

A static domain → provider table (`gmail.com`/`googlemail.com` → Gmail; `fastmail.com`/`fastmail.fm` → Fastmail; `icloud.com`/`me.com`/`mac.com` → iCloud) suggests a default provider from the email address being authenticated. When the domain is unrecognized or ambiguous (e.g. Google Workspace on a custom domain), `pigeon` prompts interactively via `dialoguer::Select` to choose a provider, falling back to a `custom` provider kind with explicit host/port when nothing matches.

This implies a **future CLI amendment** beyond ADR-0002's current `authenticate` shape (currently just `email` + `--alias`): a `--provider` flag (`gmail | fastmail | icloud | proton | custom`) and, for `custom`, `--host`/`--port`. That amendment is not made here — it's recorded so implementation work doesn't silently diverge from ADR-0002's documented argument shape without an ADR update.

### New dependencies

`async-imap`, `tokio`, `async-native-tls`, `keyring`, `dialoguer`, `serde` + `toml` (metadata file), `directories` (config path resolution).

## Consequences

- `authenticate` and `list-identities` can now be implemented for real against this design; `sink` and `transform` remain out of scope (per ADR-0002) and get their own ADR when tackled.
- No OAuth client registration, consent screen, or CASA assessment is needed for v1 — meaningfully less setup and ongoing maintenance burden than an OAuth-first design.
- Risk: Proton Bridge's self-signed local certificate needs explicit trust handling in the TLS layer, distinct from the public-CA path used for the other three providers.
- Risk: `keyring` on Linux depends on a running Secret Service daemon (e.g. `gnome-keyring`, KWallet); headless/server Linux environments without one will need a documented fallback or explicit error rather than a silent failure.
- Risk: Google could tighten app-password availability further in the future (e.g. broader Advanced Protection Program requirements); OAuth2 is the documented fallback if that happens.

## Out of scope

- `sink`/`transform` implementation.
- the OAuth2 implementation itself (deferred).

use std::time::Duration;

use async_imap::imap_proto::{Capability, Response, ResponseCode};
use async_native_tls::TlsConnector;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The concrete `async-imap` session type used throughout `pigeon`: a TLS
/// connection over a plain TCP socket. The same type whichever
/// `ImapSecurity` established it, since STARTTLS ends in the same TLS
/// stream over the same socket.
pub(crate) type ImapSession = async_imap::Session<async_native_tls::TlsStream<TcpStream>>;

/// Budget for the whole connect/TLS/greeting/LOGIN sequence. Without this,
/// a network or server stall at any point in that sequence hangs forever
/// with no output at all, since nothing is printed until this call returns.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How the IMAP connection is secured (CLI-0162). There is deliberately no
/// plaintext variant: `LOGIN` never goes over an unencrypted socket, not
/// even to a localhost Bridge.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImapSecurity {
    /// Implicit TLS from the first byte, as on port 993.
    Tls,
    /// A plaintext greeting, then `STARTTLS` before anything else, as on
    /// port 143 and Proton Mail Bridge's default 1143.
    #[value(name = "starttls")]
    #[serde(rename = "starttls")]
    StartTls,
}

impl std::fmt::Display for ImapSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ImapSecurity::Tls => "tls",
            ImapSecurity::StartTls => "starttls",
        })
    }
}

/// Connects to `host:port`, secures the connection per `security`, and logs
/// in with `email`/`secret`, returning the still-open session. Callers are
/// responsible for eventually logging out.
///
/// `accept_invalid_certs` should only be `true` for Proton Mail Bridge,
/// which terminates TLS locally with a self-signed certificate.
pub(crate) async fn connect_and_login(
    host: &str,
    port: u16,
    email: &str,
    secret: &str,
    security: ImapSecurity,
    accept_invalid_certs: bool,
) -> Result<ImapSession, String> {
    match timeout(
        CONNECT_TIMEOUT,
        connect_and_login_inner(host, port, email, secret, security, accept_invalid_certs),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "timed out connecting to {host}:{port} after {CONNECT_TIMEOUT:?}"
        )),
    }
}

async fn connect_and_login_inner(
    host: &str,
    port: u16,
    email: &str,
    secret: &str,
    security: ImapSecurity,
    accept_invalid_certs: bool,
) -> Result<ImapSession, String> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|err| format!("failed to connect to {host}:{port}: {err}"))?;

    let tcp = match security {
        ImapSecurity::Tls => tcp,
        ImapSecurity::StartTls => start_tls(host, port, tcp).await?,
    };

    let tls = TlsConnector::new()
        .danger_accept_invalid_certs(accept_invalid_certs)
        .connect(host, tcp)
        .await
        .map_err(|err| {
            let mut message = format!("TLS handshake with {host}:{port} failed: {err}");
            if security == ImapSecurity::Tls
                && let Some(hint) = tls_mode_hint(&message)
            {
                message.push_str(" -- ");
                message.push_str(hint);
            }
            message
        })?;

    let mut client = async_imap::Client::new(tls);
    // RFC 3501 §6.2.1: no greeting follows a STARTTLS negotiation, so only
    // the implicit-TLS path reads one here.
    if security == ImapSecurity::Tls {
        client
            .read_response()
            .await
            .map_err(|err| format!("failed to read greeting from {host}:{port}: {err}"))?
            .ok_or_else(|| format!("connection to {host}:{port} closed before greeting"))?;
    }

    client
        .login(email, secret)
        .await
        .map_err(|(err, _client)| format!("login failed: {err}"))
}

/// The plaintext half of STARTTLS: reads the greeting, refuses a server
/// that advertises capabilities without `STARTTLS`, then issues `STARTTLS`
/// and hands back the raw socket for the TLS handshake.
///
/// This is the downgrade guard. Every failure here is an `Err`, and nothing
/// here ever sends credentials, so a server that cannot or will not upgrade
/// gets no `LOGIN` at all, rather than one in the clear.
async fn start_tls(host: &str, port: u16, tcp: TcpStream) -> Result<TcpStream, String> {
    let mut client = async_imap::Client::new(tcp);
    let greeting = client
        .read_response()
        .await
        .map_err(|err| format!("failed to read greeting from {host}:{port}: {err}"))?
        .ok_or_else(|| format!("connection to {host}:{port} closed before greeting"))?;

    if greeting_lacks_starttls(greeting.parsed()) {
        return Err(format!(
            "{host}:{port} does not offer STARTTLS; refusing to log in over plaintext \
             (if it expects TLS from the first byte, use --security tls)"
        ));
    }

    client
        .run_command_and_check_ok("STARTTLS", None)
        .await
        .map_err(|err| format!("STARTTLS with {host}:{port} failed: {err}"))?;

    Ok(client.into_inner())
}

/// Whether a greeting carries a `[CAPABILITY ...]` code that omits
/// `STARTTLS`. A greeting with no capability code at all is not evidence
/// either way, so it returns `false` and the `STARTTLS` command itself
/// decides: a server that lacks it answers `BAD`/`NO`, which `start_tls`
/// turns into an error before any credential is sent.
fn greeting_lacks_starttls(greeting: &Response<'_>) -> bool {
    let code = match greeting {
        Response::Data { code, .. } => code,
        _ => return false,
    };
    match code {
        Some(ResponseCode::Capabilities(capabilities)) => !capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::Atom(atom) if atom.eq_ignore_ascii_case("STARTTLS"))),
        _ => false,
    }
}

/// A hint for an implicit-TLS handshake that failed because the server
/// spoke plaintext first, as a STARTTLS server does. Both strings are how a
/// TLS stack reports reading `* OK ...` as a record header: "record
/// overflow" from macOS's Security framework, "wrong version number" from
/// OpenSSL.
fn tls_mode_hint(message: &str) -> Option<&'static str> {
    let lowered = message.to_ascii_lowercase();
    (lowered.contains("record overflow") || lowered.contains("wrong version number")).then_some(
        "the server sent plaintext, so it may expect STARTTLS; retry with --security starttls",
    )
}

/// Connects to `host:port`, secures it per `security`, and attempts an
/// IMAP `LOGIN` with `email`/`secret`, then logs out. Used by `authenticate`
/// to verify a credential works before it's ever persisted to the keychain
/// or the identity metadata file.
pub fn verify_login(
    host: &str,
    port: u16,
    email: &str,
    secret: &str,
    security: ImapSecurity,
    accept_invalid_certs: bool,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(async {
        let mut session =
            connect_and_login(host, port, email, secret, security, accept_invalid_certs).await?;
        // Best-effort (CLI-0068): LOGIN already succeeded by this point, so
        // a logout-time disconnect must not make a valid credential look
        // rejected.
        let _ = session.logout().await;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[test]
    fn tls_mode_hint_matches_both_plaintext_signatures_and_nothing_else() {
        assert!(
            tls_mode_hint("TLS handshake with 127.0.0.1:1143 failed: record overflow").is_some()
        );
        assert!(tls_mode_hint("ssl routines: WRONG VERSION NUMBER").is_some());
        assert!(tls_mode_hint("TLS handshake failed: certificate has expired").is_none());
        assert!(tls_mode_hint("connection reset by peer").is_none());
    }

    /// A fake IMAP server that writes `greeting`, answers `STARTTLS` with
    /// `starttls_reply` (if the client sends it), and records every line it
    /// receives. Returns its port and the lines.
    async fn fake_server(
        greeting: &'static str,
        starttls_reply: &'static str,
    ) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            write.write_all(greeting.as_bytes()).await.unwrap();
            let mut lines = BufReader::new(read).lines();
            let mut seen = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some((tag, rest)) = line.split_once(' ')
                    && rest.eq_ignore_ascii_case("STARTTLS")
                {
                    let reply = starttls_reply.replace("{tag}", tag);
                    let _ = write.write_all(reply.as_bytes()).await;
                }
                seen.push(line);
            }
            seen
        });
        (port, handle)
    }

    async fn assert_refused_without_login(greeting: &'static str, starttls_reply: &'static str) {
        let (port, server) = fake_server(greeting, starttls_reply).await;
        let result = connect_and_login(
            "127.0.0.1",
            port,
            "someone@example.com",
            "hunter2",
            ImapSecurity::StartTls,
            false,
        )
        .await;
        let Err(err) = result else {
            panic!("STARTTLS failure must be an error");
        };
        let seen = server.await.unwrap();
        assert!(
            seen.iter()
                .all(|line| !line.to_ascii_uppercase().contains("LOGIN")),
            "no LOGIN may ever be sent before TLS; error was {err:?}, server saw {seen:?}"
        );
        assert!(
            seen.iter().all(|line| !line.contains("hunter2")),
            "the secret must never reach a plaintext socket"
        );
    }

    #[tokio::test]
    async fn starttls_refuses_a_server_whose_greeting_omits_it() {
        assert_refused_without_login(
            "* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] ready\r\n",
            "{tag} OK begin TLS\r\n",
        )
        .await;
    }

    #[tokio::test]
    async fn starttls_refuses_a_server_that_rejects_the_command() {
        assert_refused_without_login("* OK ready\r\n", "{tag} BAD unknown command\r\n").await;
    }

    #[test]
    fn imap_security_round_trips_through_its_stored_spelling() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            security: ImapSecurity,
        }
        let encoded = toml::to_string(&Wrapper {
            security: ImapSecurity::StartTls,
        })
        .unwrap();
        assert_eq!(encoded.trim(), "security = \"starttls\"");
        let decoded: Wrapper = toml::from_str("security = \"tls\"").unwrap();
        assert_eq!(decoded.security, ImapSecurity::Tls);
    }
}

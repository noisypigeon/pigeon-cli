use std::time::Duration;

use async_native_tls::TlsConnector;
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The concrete `async-imap` session type used throughout `pigeon`: a TLS
/// connection over a plain TCP socket.
pub(crate) type ImapSession = async_imap::Session<async_native_tls::TlsStream<TcpStream>>;

/// Budget for the whole connect/TLS/greeting/LOGIN sequence. Without this,
/// a network or server stall at any point in that sequence hangs forever
/// with no output at all, since nothing is printed until this call returns.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connects to `host:port` over TLS and logs in with `email`/`secret`,
/// returning the still-open session. Callers are responsible for eventually
/// logging out.
///
/// `accept_invalid_certs` should only be `true` for Proton Mail Bridge,
/// which terminates TLS locally with a self-signed certificate.
pub(crate) async fn connect_and_login(
    host: &str,
    port: u16,
    email: &str,
    secret: &str,
    accept_invalid_certs: bool,
) -> Result<ImapSession, String> {
    match timeout(
        CONNECT_TIMEOUT,
        connect_and_login_inner(host, port, email, secret, accept_invalid_certs),
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
    accept_invalid_certs: bool,
) -> Result<ImapSession, String> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|err| format!("failed to connect to {host}:{port}: {err}"))?;

    let tls = TlsConnector::new()
        .danger_accept_invalid_certs(accept_invalid_certs)
        .connect(host, tcp)
        .await
        .map_err(|err| format!("TLS handshake with {host}:{port} failed: {err}"))?;

    let mut client = async_imap::Client::new(tls);
    client
        .read_response()
        .await
        .map_err(|err| format!("failed to read greeting from {host}:{port}: {err}"))?
        .ok_or_else(|| format!("connection to {host}:{port} closed before greeting"))?;

    client
        .login(email, secret)
        .await
        .map_err(|(err, _client)| format!("login failed: {err}"))
}

/// Connects to `host:port` over TLS and attempts an IMAP `LOGIN` with
/// `email`/`secret`, then logs out. Used by `authenticate` to verify a
/// credential works before it's ever persisted to the keychain or the
/// identity metadata file.
pub fn verify_login(
    host: &str,
    port: u16,
    email: &str,
    secret: &str,
    accept_invalid_certs: bool,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(async {
        let mut session =
            connect_and_login(host, port, email, secret, accept_invalid_certs).await?;
        session
            .logout()
            .await
            .map_err(|err| format!("logout failed: {err}"))
    })
}

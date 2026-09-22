use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use dialoguer::Password;

use super::FAILURE_EXIT_CODE;
use crate::cli::EmailCommands;
use crate::identity::{self, Identity, Store};
use crate::provider::Provider;
use crate::{credentials, imap_client};

pub fn dispatch(command: EmailCommands) -> i32 {
    match command {
        EmailCommands::Authenticate {
            email,
            alias,
            provider,
            host,
            port,
        } => authenticate(email, alias, provider, host, port),
        EmailCommands::ListIdentities => list_identities(),
        EmailCommands::Sink { alias, directory } => sink(alias, directory),
        EmailCommands::Transform {
            alias,
            input,
            output,
        } => transform(alias, input, output),
    }
}

fn authenticate(
    email: String,
    alias: Option<String>,
    provider: Option<Provider>,
    host: Option<String>,
    port: Option<u16>,
) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let mut store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let alias = alias.unwrap_or_else(|| identity::sanitize_alias(&email));
    if store.contains_alias(&alias) {
        return fail(format!(
            "an identity with alias '{alias}' is already authenticated"
        ));
    }

    let provider = match provider.or_else(|| Provider::detect(&email)) {
        Some(provider) => provider,
        None => match Provider::prompt_select() {
            Ok(provider) => provider,
            Err(err) => return fail(format!("failed to read provider selection: {err}")),
        },
    };

    let (host, port) = match resolve_host_port(provider, host, port) {
        Ok(host_port) => host_port,
        Err(err) => return fail(err),
    };

    let secret = match read_secret(&email) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    if let Err(err) = imap_client::verify_login(
        &host,
        port,
        &email,
        &secret,
        provider.accepts_invalid_certs(),
    ) {
        return fail(err);
    }

    if let Err(err) = credentials::set_secret(&alias, &secret) {
        return fail(err);
    }

    store.push(Identity {
        alias: alias.clone(),
        email: email.clone(),
        provider,
        host,
        port,
    });
    if let Err(err) = store.save(&path) {
        // The keychain write already succeeded; don't leave an orphaned
        // secret behind if the metadata write failed.
        let _ = credentials::delete_secret(&alias);
        return fail(err);
    }

    println!("Authenticated {email} as '{alias}' ({provider}).");
    0
}

/// Resolves the IMAP host/port to use: explicit `--host`/`--port` win, then
/// the provider's own default. `Custom` has no default, so `--host` becomes
/// required at this point; `--port` falls back to 993 (the common IMAPS port).
fn resolve_host_port(
    provider: Provider,
    host: Option<String>,
    port: Option<u16>,
) -> Result<(String, u16), String> {
    let defaults = provider.default_host_port();
    let host = host
        .or_else(|| defaults.map(|(host, _)| host.to_string()))
        .ok_or_else(|| format!("--host is required for provider '{provider}'"))?;
    let port = port.or(defaults.map(|(_, port)| port)).unwrap_or(993);
    Ok((host, port))
}

/// Reads the app/bridge password. Masked and interactive on a real TTY;
/// falls back to a plain line read from stdin otherwise, so a secret can be
/// piped in (e.g. from a password manager), which also makes this testable
/// without a terminal.
fn read_secret(email: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        Password::new()
            .with_prompt(format!("App password for {email}"))
            .interact()
            .map_err(|err| format!("failed to read secret: {err}"))
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("failed to read secret from stdin: {err}"))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

fn list_identities() -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    if store.is_empty() {
        println!("No identities configured.");
        return 0;
    }

    for identity in store.iter() {
        println!(
            "{}\t{}\t{}",
            identity.alias, identity.email, identity.provider
        );
    }
    0
}

fn sink(alias: Option<String>, directory: PathBuf) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let identity = match &alias {
        Some(alias) => match store.iter().find(|identity| &identity.alias == alias) {
            Some(identity) => identity,
            None => return fail(format!("no identity with alias '{alias}'")),
        },
        None => match store.prompt_select() {
            Ok(identity) => identity,
            Err(err) => return fail(err),
        },
    };

    let secret = match credentials::get_secret(&identity.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    match crate::sink::run(
        &identity.email,
        &identity.host,
        identity.port,
        &secret,
        identity.provider.accepts_invalid_certs(),
        &directory,
    ) {
        Ok(summary) => {
            println!(
                "Sunk {} identity across {} mailbox(es): {} message(s) downloaded, {} already present.",
                identity.alias, summary.mailboxes, summary.downloaded, summary.already_present
            );
            0
        }
        Err(err) => fail(err),
    }
}

fn transform(alias: Option<String>, input: PathBuf, output: PathBuf) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let identity = match &alias {
        Some(alias) => match store.iter().find(|identity| &identity.alias == alias) {
            Some(identity) => identity,
            None => return fail(format!("no identity with alias '{alias}'")),
        },
        None => match store.prompt_select() {
            Ok(identity) => identity,
            Err(err) => return fail(err),
        },
    };

    match crate::transform::run(identity, &input, &output) {
        Ok(summary) => {
            println!(
                "Transformed {} identity: {} message(s), {} attachment(s), {} skipped.",
                identity.alias, summary.messages, summary.attachments, summary.skipped
            );
            0
        }
        Err(err) => fail(err),
    }
}

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

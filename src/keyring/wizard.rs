use std::io::{BufRead, IsTerminal};

use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};

use crate::commands::{FAILURE_EXIT_CODE, print_table};
use crate::dataops::client;
use crate::dataops::store::BucketConfig;
use crate::email::identity::{self, Identity};
use crate::email::imap_client;
use crate::email::provider::Provider;
use crate::keyring::cli::AddKind;
use crate::keyring::credentials;
use crate::keyring::store::{KeyringEntry, Store};

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

/// Reads a secret. Masked and interactive on a real TTY; falls back to a
/// plain line read from stdin otherwise, so a secret can be piped in (e.g.
/// from a password manager). Merges `email::commands::read_secret`/
/// `dataops::commands::read_secret`, which were byte-for-byte the same
/// shape.
fn read_secret(prompt: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        Password::new()
            .with_prompt(prompt)
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

/// Asks a yes/no question. Interactive on a real TTY; falls back to reading
/// a plain `y`/`n` line from stdin otherwise. From `dataops::commands::confirm`
/// (`email` had no confirm helper before this, since it never had a
/// destructive command needing one).
fn confirm(prompt: &str, default: bool) -> Result<bool, String> {
    if std::io::stdin().is_terminal() {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .default(default)
            .interact()
            .map_err(|err| format!("failed to read confirmation: {err}"))
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("failed to read confirmation from stdin: {err}"))?;
        Ok(match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        })
    }
}

/// Resolves the IMAP host/port to use: explicit `--host`/`--port` win, then
/// the provider's own default. `Custom` has no default, so `--host` becomes
/// required at this point; `--port` falls back to 993 (the common IMAPS
/// port). Moved verbatim from `email::commands`.
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

/// Runs `dataops::client::bucket_exists` on its own short-lived async
/// runtime -- `keyring::wizard`'s top-level functions stay synchronous
/// (matching `email::imap_client::verify_login`'s own self-contained-runtime
/// shape), since a single dispatch entry point needs to reach both this
/// (async) and `verify_login` (which builds its own nested runtime and
/// would panic if called from inside an already-running one).
fn check_bucket_exists(bucket_config: &BucketConfig, secret: &str) -> Result<bool, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;
    runtime.block_on(client::bucket_exists(bucket_config, secret))
}

/// `pigeon keyring add [email|bucket]`: dispatches on the given subcommand,
/// or -- for a bare `pigeon keyring add` -- prompts `Select` "Email or
/// Bucket?" first and runs the same branch with every field unset.
pub fn add(kind: Option<AddKind>) -> i32 {
    match kind {
        Some(AddKind::Email {
            email,
            alias,
            provider,
            host,
            port,
        }) => add_email(Some(email), alias, provider, host, port),
        Some(AddKind::Bucket { alias }) => add_bucket(alias),
        None => match Select::with_theme(&ColorfulTheme::default())
            .with_prompt("What would you like to add?")
            .items(["Email identity", "Bucket-config"])
            .default(0)
            .interact()
        {
            Ok(0) => add_email(None, None, None, None, None),
            Ok(_) => add_bucket(None),
            Err(err) => fail(format!("failed to read selection: {err}")),
        },
    }
}

fn add_email(
    email: Option<String>,
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

    let email = match email {
        Some(email) => email,
        None => match Input::<String>::new()
            .with_prompt("Email address")
            .interact_text()
        {
            Ok(email) => email,
            Err(err) => return fail(format!("failed to read email address: {err}")),
        },
    };

    let alias = alias.unwrap_or_else(|| identity::sanitize_alias(&email));
    if store.contains_alias(&alias) {
        return fail(format!("an entry named '{alias}' already exists"));
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

    let secret = match read_secret(&format!("App password for {email}")) {
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

    store.push(KeyringEntry::Email(Identity {
        alias: alias.clone(),
        email: email.clone(),
        provider,
        host,
        port,
    }));
    if let Err(err) = store.save(&path) {
        // The keychain write already succeeded; don't leave an orphaned
        // secret behind if the metadata write failed.
        let _ = credentials::delete_secret(&alias);
        return fail(err);
    }

    println!("Added {email} as '{alias}' ({provider}).");
    0
}

fn add_bucket(alias: Option<String>) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let mut store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let alias = match alias {
        Some(alias) => alias,
        None => match Input::<String>::new().with_prompt("Alias").interact_text() {
            Ok(alias) => alias,
            Err(err) => return fail(format!("failed to read alias: {err}")),
        },
    };
    if store.contains_alias(&alias) {
        return fail(format!("an entry named '{alias}' already exists"));
    }

    let bucket = match Input::<String>::new()
        .with_prompt("Bucket Name")
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read bucket name: {err}")),
    };

    let endpoint = match Input::<String>::new()
        .with_prompt("Endpoint URL")
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read endpoint: {err}")),
    };

    let access_key_id = match Input::<String>::new()
        .with_prompt("Access Key ID")
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read access key ID: {err}")),
    };

    let secret_key = match read_secret("Secret Key") {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    let candidate = BucketConfig {
        alias: alias.clone(),
        endpoint,
        bucket,
        access_key_id,
    };

    match check_bucket_exists(&candidate, &secret_key) {
        Ok(true) => {}
        Ok(false) => return fail(format!("bucket '{}' does not exist", candidate.bucket)),
        Err(err) => return fail(format!("failed to verify bucket: {err}")),
    }

    if let Err(err) = credentials::set_secret(&alias, &secret_key) {
        return fail(err);
    }

    store.push(KeyringEntry::Bucket(candidate));
    if let Err(err) = store.save(&path) {
        let _ = credentials::delete_secret(&alias);
        return fail(err);
    }

    println!("Added bucket-config '{alias}'.");
    0
}

/// `pigeon keyring modify [ALIAS]`: resolves the target entry (by alias, or
/// interactively among every configured entry when omitted), then edits it
/// in place -- the bucket branch matches `dataops bucket-config edit`'s
/// existing shape exactly; the email branch is new, since no identity-edit
/// capability existed anywhere before this.
pub fn modify(alias: Option<String>) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let mut store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let alias = match resolve_existing_alias(&store, alias) {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    let entry = store.find(&alias).unwrap().clone();

    match entry {
        KeyringEntry::Email(identity) => modify_email(&mut store, &path, &identity),
        KeyringEntry::Bucket(bucket_config) => modify_bucket(&mut store, &path, &bucket_config),
    }
}

fn modify_email(store: &mut Store, path: &std::path::Path, current: &Identity) -> i32 {
    let providers = [
        Provider::Gmail,
        Provider::Fastmail,
        Provider::Icloud,
        Provider::Proton,
        Provider::Custom,
    ];
    let labels: Vec<String> = providers.iter().map(Provider::to_string).collect();
    let current_index = providers
        .iter()
        .position(|provider| *provider == current.provider)
        .unwrap_or(0);
    let provider = match Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Provider")
        .items(&labels)
        .default(current_index)
        .interact()
    {
        Ok(index) => providers[index],
        Err(err) => return fail(format!("failed to read provider selection: {err}")),
    };

    let host = match Input::<String>::new()
        .with_prompt("Host")
        .default(current.host.clone())
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read host: {err}")),
    };

    let port = match Input::<u16>::new()
        .with_prompt("Port")
        .default(current.port)
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read port: {err}")),
    };

    let new_secret = match read_secret("App password (press enter to keep current)") {
        Ok(secret) if secret.is_empty() => None,
        Ok(secret) => Some(secret),
        Err(err) => return fail(err),
    };

    let secret = match &new_secret {
        Some(secret) => secret.clone(),
        None => match credentials::get_secret(&current.alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        },
    };

    if let Err(err) = imap_client::verify_login(
        &host,
        port,
        &current.email,
        &secret,
        provider.accepts_invalid_certs(),
    ) {
        return fail(err);
    }

    if let Some(secret) = &new_secret
        && let Err(err) = credentials::set_secret(&current.alias, secret)
    {
        return fail(err);
    }

    let updated = Identity {
        alias: current.alias.clone(),
        email: current.email.clone(),
        provider,
        host,
        port,
    };
    store.remove(&current.alias);
    store.push(KeyringEntry::Email(updated));
    if let Err(err) = store.save(path) {
        return fail(err);
    }

    println!("Updated '{}'.", current.alias);
    0
}

fn modify_bucket(store: &mut Store, path: &std::path::Path, current: &BucketConfig) -> i32 {
    let bucket = match Input::<String>::new()
        .with_prompt("Bucket Name")
        .default(current.bucket.clone())
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read bucket name: {err}")),
    };

    let endpoint = match Input::<String>::new()
        .with_prompt("Endpoint URL")
        .default(current.endpoint.clone())
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read endpoint: {err}")),
    };

    let access_key_id = match Input::<String>::new()
        .with_prompt("Access Key ID")
        .default(current.access_key_id.clone())
        .interact_text()
    {
        Ok(value) => value,
        Err(err) => return fail(format!("failed to read access key ID: {err}")),
    };

    let new_secret = match read_secret("Secret Key (press enter to keep current)") {
        Ok(secret) if secret.is_empty() => None,
        Ok(secret) => Some(secret),
        Err(err) => return fail(err),
    };

    let secret_key = match &new_secret {
        Some(secret) => secret.clone(),
        None => match credentials::get_secret(&current.alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        },
    };

    let candidate = BucketConfig {
        alias: current.alias.clone(),
        endpoint,
        bucket,
        access_key_id,
    };

    match check_bucket_exists(&candidate, &secret_key) {
        Ok(true) => {}
        Ok(false) => return fail(format!("bucket '{}' does not exist", candidate.bucket)),
        Err(err) => return fail(format!("failed to verify bucket: {err}")),
    }

    if let Some(secret) = &new_secret
        && let Err(err) = credentials::set_secret(&current.alias, secret)
    {
        return fail(err);
    }

    store.remove(&current.alias);
    store.push(KeyringEntry::Bucket(candidate));
    if let Err(err) = store.save(path) {
        return fail(err);
    }

    println!("Updated '{}'.", current.alias);
    0
}

/// `pigeon keyring delete <ALIAS>`: confirms, then removes the entry and
/// its keychain secret. The alias being passed directly on the command
/// line doesn't waive confirmation, matching `dataops bucket-config
/// remove`'s existing behavior exactly.
pub fn delete(alias: String) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let mut store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    if !store.contains_alias(&alias) {
        return fail(format!("no entry named '{alias}'"));
    }

    match confirm(&format!("Remove '{alias}'?"), false) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    store.remove(&alias);
    if let Err(err) = store.save(&path) {
        return fail(err);
    }
    if let Err(err) = credentials::delete_secret(&alias) {
        return fail(err);
    }

    println!("Removed '{alias}'.");
    0
}

/// `pigeon keyring list`: one table, both kinds together.
pub fn list() -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    if store.is_empty() {
        println!("No keyring entries configured.");
        return 0;
    }

    let rows: Vec<Vec<String>> = store
        .iter()
        .map(|entry| match entry {
            KeyringEntry::Email(identity) => vec![
                "email".to_string(),
                identity.alias.clone(),
                format!("{} ({})", identity.email, identity.provider),
            ],
            KeyringEntry::Bucket(bucket_config) => vec![
                "bucket".to_string(),
                bucket_config.alias.clone(),
                format!("{} ({})", bucket_config.endpoint, bucket_config.bucket),
            ],
        })
        .collect();
    print_table(&["KIND", "ALIAS", "DETAIL"], &rows);
    0
}

/// Resolves an alias that must already exist: the given `alias` if it's a
/// known entry, an error if it's given but unknown, or an interactive
/// selection when omitted. Shared by `modify` (`delete` uses its own
/// simpler check since it never offers interactive selection).
fn resolve_existing_alias(store: &Store, alias: Option<String>) -> Result<String, String> {
    match alias {
        Some(alias) if store.contains_alias(&alias) => Ok(alias),
        Some(alias) => Err(format!("no entry named '{alias}'")),
        None => store.prompt_select().map(|entry| entry.alias().to_string()),
    }
}

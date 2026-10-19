use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::cli::AddKind;
use crate::commands::keyring::email::identity::{self, Identity};
use crate::commands::keyring::email::imap_client;
use crate::commands::keyring::email::provider::Provider;
use crate::commands::keyring::store::{Entry, Store};
use crate::commands::{FAILURE_EXIT_CODE, print_table};
use crate::core::keyring::KeyringEntry as _;
use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::Lsjson;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::{confirm, read_secret};

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
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

/// Verifies a candidate bucket-config is reachable before it is saved
/// (CLI-0010), via `rclone lsjson` against a throwaway config holding only
/// that candidate (CLI-0148 §3).
///
/// The ordering problem this solves: the generated `rclone.conf` is a pure
/// function of the *saved* keyring (CLI-0136), but this runs before the
/// candidate is saved. So an ephemeral one-entry `Store` is handed to the
/// real `write_sandboxed_config` -- not a hand-rolled emitter -- so
/// CLI-0140's two-section `alias` -> bucket shape cannot drift out of sync
/// with the one jobs actually use.
///
/// The probe file lives beside `keyring.toml` rather than in the OS temp
/// directory: it carries a secret access key, and that directory is the one
/// location already trusted with those at `0600` (CLI-0135). It is named
/// per-pid so two concurrent `keyring add`s cannot clobber each other, and
/// removed on every path.
///
/// Returns `Ok(())` when the bucket listed. Unlike the `minio` `HeadBucket`
/// this replaced, there is no absent-vs-unauthorised distinction to return:
/// `lsjson` either succeeds or errors, and recovering the difference would
/// mean matching rclone's stderr, which is issue #63's fragility. rclone's
/// own message does name `NoSuchBucket` when that is the cause.
///
/// Runs on its own short-lived runtime so `keyring::wizard`'s top-level
/// functions stay synchronous -- the same self-contained shape
/// `imap_client::verify_login` uses, and necessary because one dispatch
/// entry point reaches both.
fn check_bucket_reachable(bucket_config: &BucketConfig) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(async {
        crate::core::rclone::check_available().await.map_err(|_| {
            "'rclone' was not found on PATH -- required to verify a bucket-config".to_string()
        })?;

        let config_path = write_probe_config(bucket_config)?;

        let result = (Lsjson {
            path: RemoteTarget::bucket_root(&bucket_config.alias).address(),
            // Deliberately not `missing_ok` (CLI-0140): a bucket rclone
            // cannot resolve is the whole thing being checked for.
            missing_ok: false,
            config_path: Some(config_path.clone()),
            job: "keyring",
            ..Default::default()
        })
        .run()
        .await;

        let _ = std::fs::remove_file(&config_path);
        result.map(|_| ())
    })
}

/// Writes the throwaway one-entry config `check_bucket_reachable` probes
/// against, returning its path.
///
/// Split out from the probe so the filesystem half is testable without
/// invoking `rclone` -- which, with its `--retries 5
/// --low-level-retries 20` plus `Lsjson`'s own `(3, 2s)`, would turn one
/// refused connection into minutes of backoff (the test-cost constraint
/// CLI-0145 recorded).
fn write_probe_config(bucket_config: &BucketConfig) -> Result<std::path::PathBuf, String> {
    let config_path = probe_config_path()?;
    write_probe_config_at(bucket_config, &config_path)?;
    Ok(config_path)
}

/// `write_probe_config`'s body with the destination passed in, so a test
/// can exercise it against a tempdir without touching the process-global
/// state `rclone_config_path` reads -- CLI-0145's `*_from`/`*_beside`
/// precedent, and necessary rather than stylistic: `cargo test` runs these
/// in parallel threads of one process, so two tests mutating
/// `PIGEON_CONFIG_DIR` race each other.
fn write_probe_config_at(
    bucket_config: &BucketConfig,
    config_path: &std::path::Path,
) -> Result<(), String> {
    let mut probe = Store::default();
    probe.push(Entry::Bucket(bucket_config.clone()));
    crate::core::rclone::config::write_sandboxed_config(&probe, config_path)
}

/// Where `write_probe_config` puts that file: beside the generated
/// `rclone.conf`, suffixed with this process's id so two concurrent
/// `keyring add`s cannot clobber each other.
fn probe_config_path() -> Result<std::path::PathBuf, String> {
    probe_config_path_beside(&crate::core::rclone::config::rclone_config_path()?)
}

/// `probe_config_path`'s pure half: the probe file's path given where the
/// generated config lives. Same reason for the split as
/// `write_probe_config_at`.
fn probe_config_path_beside(generated: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let parent = generated
        .parent()
        .ok_or("could not determine the config directory")?;
    Ok(parent.join(format!("rclone-probe-{}.conf", std::process::id())))
}

/// `pigeon keyring add [email|bucket]`: dispatches on the
/// given subcommand, or -- for a bare `pigeon keyring add` -- prompts
/// `Select` first and runs the same branch with every field unset.
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
    let path = match Store::resolve_path() {
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

    let max_imap_connections = match confirm(
        "Cap simultaneous IMAP connections for this identity?",
        false,
    ) {
        Ok(true) => match Input::<u32>::new()
            .with_prompt("Max simultaneous IMAP connections")
            .interact_text()
        {
            Ok(value) => Some(value.max(1)),
            Err(err) => return fail(format!("failed to read max IMAP connections: {err}")),
        },
        Ok(false) => None,
        Err(err) => return fail(err),
    };

    store.push(Entry::Email(Identity {
        alias: alias.clone(),
        email: email.clone(),
        provider,
        host,
        port,
        max_imap_connections,
        secret,
    }));
    if let Err(err) = store.save(&path) {
        return fail(err);
    }

    println!("Added {email} as '{alias}' ({provider}).");
    0
}

fn add_bucket(alias: Option<String>) -> i32 {
    let path = match Store::resolve_path() {
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
        secret_key: secret_key.clone(),
    };

    if let Err(err) = check_bucket_reachable(&candidate) {
        return fail(format!(
            "failed to verify bucket '{}': {err}",
            candidate.bucket
        ));
    }

    store.push(Entry::Bucket(candidate));
    if let Err(err) = store.save(&path) {
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
    let path = match Store::resolve_path() {
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
        Entry::Email(identity) => modify_email(&mut store, &path, &identity),
        Entry::Bucket(bucket_config) => modify_bucket(&mut store, &path, &bucket_config),
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

    let secret = new_secret.unwrap_or_else(|| current.secret.clone());

    if let Err(err) = imap_client::verify_login(
        &host,
        port,
        &current.email,
        &secret,
        provider.accepts_invalid_certs(),
    ) {
        return fail(err);
    }

    let max_imap_connections = match confirm(
        "Cap simultaneous IMAP connections for this identity?",
        current.max_imap_connections.is_some(),
    ) {
        Ok(true) => {
            let mut input = Input::<u32>::new().with_prompt("Max simultaneous IMAP connections");
            if let Some(current_cap) = current.max_imap_connections {
                input = input.default(current_cap);
            }
            match input.interact_text() {
                Ok(value) => Some(value.max(1)),
                Err(err) => return fail(format!("failed to read max IMAP connections: {err}")),
            }
        }
        Ok(false) => None,
        Err(err) => return fail(err),
    };

    let updated = Identity {
        alias: current.alias.clone(),
        email: current.email.clone(),
        provider,
        host,
        port,
        max_imap_connections,
        secret,
    };
    store.remove(&current.alias);
    store.push(Entry::Email(updated));
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

    let secret_key = new_secret
        .clone()
        .unwrap_or_else(|| current.secret_key.clone());

    let candidate = BucketConfig {
        alias: current.alias.clone(),
        endpoint,
        bucket,
        access_key_id,
        secret_key: secret_key.clone(),
    };

    if let Err(err) = check_bucket_reachable(&candidate) {
        return fail(format!(
            "failed to verify bucket '{}': {err}",
            candidate.bucket
        ));
    }

    store.remove(&current.alias);
    store.push(Entry::Bucket(candidate));
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
    let path = match Store::resolve_path() {
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

    println!("Removed '{alias}'.");
    0
}

/// `pigeon keyring list`: one table, both kinds together.
pub fn list() -> i32 {
    let path = match Store::resolve_path() {
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
        .map(|entry| {
            vec![
                entry.kind().to_string(),
                entry.alias().to_string(),
                entry.detail(),
            ]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(alias: &str) -> BucketConfig {
        BucketConfig {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
            secret_key: "shh".to_string(),
        }
    }

    /// CLI-0148 §3: the probe config must carry the candidate's own two
    /// sections and nothing else, so a bucket that is not in the keyring
    /// yet is still addressable as `alias:`.
    #[test]
    fn write_probe_config_writes_only_the_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone-probe-test.conf");

        write_probe_config_at(&candidate("probe-only"), &path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[probe-only.s3]"), "{contents}");
        // CLI-0140's alias -> bucket indirection, inherited from the real
        // generator rather than re-emitted here -- which is the whole
        // reason this reuses `write_sandboxed_config`.
        assert!(
            contents.contains("remote = probe-only.s3:my-bucket"),
            "{contents}"
        );
        assert_eq!(
            contents.matches("type = s3").count(),
            1,
            "only the candidate: {contents}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "carries a secret key (CLI-0135)");
        }
    }

    /// Named per-pid and placed beside the generated config, not in the OS
    /// temp directory -- it holds a secret access key, and the keyring's own
    /// directory is the one already trusted with those at 0600.
    #[test]
    fn probe_config_path_sits_beside_the_generated_config_and_names_the_pid() {
        let generated = std::path::Path::new("/etc/pigeon/rclone.conf");

        let probe = probe_config_path_beside(generated).unwrap();

        assert_eq!(probe.parent(), generated.parent());
        assert_eq!(
            probe.file_name().unwrap().to_str().unwrap(),
            format!("rclone-probe-{}.conf", std::process::id())
        );
    }
}

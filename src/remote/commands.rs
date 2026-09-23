use std::fs;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, Password, theme::ColorfulTheme};

use crate::commands::FAILURE_EXIT_CODE;
use crate::remote::cli::RemoteCommands;
use crate::remote::location::{self, Location};
use crate::remote::store::{Remote, Store};
use crate::remote::{client, credentials};

pub fn dispatch(command: RemoteCommands) -> i32 {
    match command {
        RemoteCommands::Configure { alias } => configure(alias),
        RemoteCommands::List => list_remotes(),
        RemoteCommands::Edit { alias } => edit(alias),
        RemoteCommands::Remove { alias } => remove(alias),
        RemoteCommands::ListBuckets { alias } => list_buckets(alias),
        RemoteCommands::Ls { location } => list(location, true),
        RemoteCommands::Lsd { location } => list(location, false),
        RemoteCommands::Copy { source, dest } => copy(source, dest),
    }
}

fn configure(alias: Option<String>) -> i32 {
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
        return fail(format!("a remote named '{alias}' already exists"));
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

    let candidate = Remote {
        alias: alias.clone(),
        endpoint,
        bucket,
        access_key_id,
    };

    match client::bucket_exists(&candidate, &secret_key) {
        Ok(true) => {}
        Ok(false) => return fail(format!("bucket '{}' does not exist", candidate.bucket)),
        Err(err) => return fail(format!("failed to verify bucket: {err}")),
    }

    if let Err(err) = credentials::set_secret(&alias, &secret_key) {
        return fail(err);
    }

    store.push(candidate);
    if let Err(err) = store.save(&path) {
        // The keychain write already succeeded; don't leave an orphaned
        // secret behind if the metadata write failed.
        let _ = credentials::delete_secret(&alias);
        return fail(err);
    }

    println!("Configured remote '{alias}'.");
    0
}

/// Reads the S3 secret access key. Masked and interactive on a real TTY;
/// falls back to a plain line read from stdin otherwise -- duplicated from
/// `email::commands::read_secret`'s shape rather than shared, since `email`
/// and `remote` are meant to stay independent siblings per ADR-0008 and this
/// is a small enough function that the duplication costs little.
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
/// a plain `y`/`n` line from stdin otherwise (`dialoguer::Confirm`, like
/// `Password`, errors outside a TTY) -- `default` applies to an empty or
/// unrecognized answer in the fallback path, same as it would for enter on
/// the interactive prompt.
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

fn list_remotes() -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    if store.is_empty() {
        println!("No remotes configured.");
        return 0;
    }

    for remote in store.iter() {
        println!("{}\t{}\t{}", remote.alias, remote.endpoint, remote.bucket);
    }
    0
}

fn edit(alias: Option<String>) -> i32 {
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
    let current = store.find(&alias).unwrap().clone();

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
        None => match credentials::get_secret(&alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        },
    };

    let candidate = Remote {
        alias: alias.clone(),
        endpoint,
        bucket,
        access_key_id,
    };

    match client::bucket_exists(&candidate, &secret_key) {
        Ok(true) => {}
        Ok(false) => return fail(format!("bucket '{}' does not exist", candidate.bucket)),
        Err(err) => return fail(format!("failed to verify bucket: {err}")),
    }

    if let Some(secret) = &new_secret
        && let Err(err) = credentials::set_secret(&alias, secret)
    {
        return fail(err);
    }

    *store.find_mut(&alias).unwrap() = candidate;
    if let Err(err) = store.save(&path) {
        return fail(err);
    }

    println!("Updated remote '{alias}'.");
    0
}

fn remove(alias: Option<String>) -> i32 {
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

    match confirm(&format!("Remove remote '{alias}'?"), false) {
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

    println!("Removed remote '{alias}'.");
    0
}

/// Resolves an alias that must already exist: the given `alias` if it's a
/// known remote, an error if it's given but unknown, or an interactive
/// selection when omitted. Shared by `edit` and `remove`.
fn resolve_existing_alias(store: &Store, alias: Option<String>) -> Result<String, String> {
    match alias {
        Some(alias) if store.contains_alias(&alias) => Ok(alias),
        Some(alias) => Err(format!("no remote named '{alias}'")),
        None => store.prompt_select().map(|remote| remote.alias.clone()),
    }
}

fn list_buckets(alias: Option<String>) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let remote = match &alias {
        Some(alias) => match store.find(alias) {
            Some(remote) => remote,
            None => return fail(format!("no remote named '{alias}'")),
        },
        None => match store.prompt_select() {
            Ok(remote) => remote,
            Err(err) => return fail(err),
        },
    };

    let secret = match credentials::get_secret(&remote.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    match client::list_buckets(remote, &secret) {
        Ok(buckets) if buckets.is_empty() => {
            println!("No buckets found.");
            0
        }
        Ok(buckets) => {
            for bucket in buckets {
                println!("{bucket}");
            }
            0
        }
        Err(err) => fail(err),
    }
}

fn list(location_arg: String, recursive: bool) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let (remote, prefix) = match location::parse(&location_arg, &store) {
        Location::Remote { alias, path } => match store.find(&alias) {
            Some(remote) => (remote, path),
            None => return fail(format!("no remote named '{alias}'")),
        },
        Location::Local(_) => {
            return fail("ls/lsd only operate on a configured remote, e.g. 'email:path'");
        }
    };

    let secret = match credentials::get_secret(&remote.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    match client::list_objects(remote, &secret, &prefix, recursive) {
        Ok(entries) => {
            for entry in entries {
                if entry.is_prefix {
                    println!("{}", entry.key);
                } else {
                    println!("{:>12} {}", entry.size, entry.key);
                }
            }
            0
        }
        Err(err) => fail(err),
    }
}

fn copy(source: String, dest: String) -> i32 {
    let path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let store = match Store::load(&path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let source_loc = location::parse(&source, &store);
    let dest_loc = location::parse(&dest, &store);

    match (source_loc, dest_loc) {
        (
            Location::Local(local),
            Location::Remote {
                alias,
                path: remote_path,
            },
        ) => upload(&store, &local, &alias, &remote_path),
        (
            Location::Remote {
                alias,
                path: remote_path,
            },
            Location::Local(local),
        ) => download(&store, &alias, &remote_path, &local),
        (Location::Remote { .. }, Location::Remote { .. }) => {
            fail("remote-to-remote copy is not supported")
        }
        (Location::Local(_), Location::Local(_)) => {
            fail("at least one of SOURCE/DEST must be a remote (alias:path)")
        }
    }
}

fn upload(store: &Store, local: &Path, remote_alias: &str, remote_path: &str) -> i32 {
    let remote = match store.find(remote_alias) {
        Some(remote) => remote,
        None => return fail(format!("no remote named '{remote_alias}'")),
    };
    let secret = match credentials::get_secret(&remote.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    let files = match collect_local_files(local) {
        Ok(files) => files,
        Err(err) => return fail(err),
    };
    let is_single_file = files.len() == 1 && local.is_file();

    let mut uploaded = 0;
    let mut unchanged = 0;
    for file in &files {
        let key = if is_single_file {
            single_file_key(remote_path, file)
        } else {
            let relative = file.strip_prefix(local).unwrap_or(file);
            join_key(remote_path, &relative.to_string_lossy())
        };

        let data = match fs::read(file) {
            Ok(data) => data,
            Err(err) => return fail(format!("failed to read {}: {err}", file.display())),
        };
        match client::upload_if_changed(remote, &secret, &key, data) {
            Ok(client::UploadOutcome::Uploaded) => uploaded += 1,
            Ok(client::UploadOutcome::Unchanged) => unchanged += 1,
            Err(err) => return fail(err),
        }
    }
    println!(
        "Uploaded {uploaded} file(s), {unchanged} unchanged, to '{remote_alias}:{remote_path}'."
    );
    0
}

fn download(store: &Store, remote_alias: &str, remote_path: &str, local: &Path) -> i32 {
    let remote = match store.find(remote_alias) {
        Some(remote) => remote,
        None => return fail(format!("no remote named '{remote_alias}'")),
    };
    let secret = match credentials::get_secret(&remote.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    let entries = match client::list_objects(remote, &secret, remote_path, true) {
        Ok(entries) => entries,
        Err(err) => return fail(err),
    };
    if entries.is_empty() {
        return fail(format!(
            "no objects found under '{remote_alias}:{remote_path}'"
        ));
    }
    let single = entries.len() == 1 && entries[0].key == remote_path;

    let mut count = 0;
    for entry in &entries {
        let data = match client::get_object(remote, &secret, &entry.key) {
            Ok(data) => data,
            Err(err) => return fail(err),
        };

        let dest_path = if single {
            local.to_path_buf()
        } else {
            let relative = entry
                .key
                .strip_prefix(remote_path)
                .unwrap_or(&entry.key)
                .trim_start_matches('/');
            local.join(relative)
        };

        if let Some(parent) = dest_path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            return fail(format!("failed to create {}: {err}", parent.display()));
        }
        if let Err(err) = fs::write(&dest_path, data) {
            return fail(format!("failed to write {}: {err}", dest_path.display()));
        }
        count += 1;
    }
    println!(
        "Downloaded {count} file(s) from '{remote_alias}:{remote_path}' to {}.",
        local.display()
    );
    0
}

/// Recursively collects every file under `local` (or just `local` itself if
/// it's a single file).
fn collect_local_files(local: &Path) -> Result<Vec<PathBuf>, String> {
    if local.is_dir() {
        let mut files = Vec::new();
        visit_dir(local, &mut files)?;
        files.sort();
        Ok(files)
    } else if local.is_file() {
        Ok(vec![local.to_path_buf()])
    } else {
        Err(format!("{} does not exist", local.display()))
    }
}

fn visit_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

/// The S3 key for a single-file upload: `remote_path` verbatim when it looks
/// like an exact destination key, or `remote_path` + the file's own name
/// when `remote_path` is empty or looks like a directory (ends with `/`).
fn single_file_key(remote_path: &str, file: &Path) -> String {
    if remote_path.is_empty() || remote_path.ends_with('/') {
        let file_name = file
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        join_key(remote_path, &file_name)
    } else {
        remote_path.to_string()
    }
}

fn join_key(remote_path: &str, suffix: &str) -> String {
    let base = remote_path.trim_end_matches('/');
    if base.is_empty() {
        suffix.to_string()
    } else {
        format!("{base}/{suffix}")
    }
}

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

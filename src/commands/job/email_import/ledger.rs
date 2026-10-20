//! The durable ingest ledger (CLI-0164): a per-identity record of every
//! message `email import` has ever delivered, kept in `--ledger-bucket`
//! rather than only in the local `.pushed` checkpoint.
//!
//! The ledger is a **snapshot of ingestion**, not a mirror of the
//! destination. Rows are only ever added: a server-side delete, a
//! `UIDVALIDITY` reset (the old rows simply stop matching), or a downstream
//! `deduplicate`/`transform` rewriting the delivered objects never removes
//! one. That is what lets the destination change shape after delivery
//! without anything being ingested twice (CLI-0164 §3).
//!
//! Layout under `--ledger-bucket`:
//!
//! ```text
//! <ledger>/<sanitized-email>/ingested.tsv     mailbox \t uidvalidity \t uid \t ingested_at
//! <ledger>/<sanitized-email>/scan-status.tsv  `email scan`'s latest snapshot
//! ```
//!
//! Both files are moved with `CopyTo` in either direction, never with
//! upload metrics: state about a run must not inflate
//! `pigeon_upload_bytes_total` (CLI-0148's reasoning for the debug-log
//! artifacts).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::commands::keyring::email::identity;
use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::{CopyTo, Lsjson, LsjsonEntry};
use crate::core::rclone::config::RemoteTarget;

/// The ledger's object name inside an identity's directory.
pub(crate) const LEDGER_FILE: &str = "ingested.tsv";

const LEDGER_HEADER: &str = "# mailbox\tuidvalidity\tuid\tingested_at";

/// One ingested `(mailbox, UIDVALIDITY, UID)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LedgerEntry {
    pub mailbox: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub ingested_at: String,
}

/// Every ingested message for one identity, keyed so serialization is
/// deterministic and a duplicate row collapses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Ledger {
    entries: BTreeMap<(String, u32, u32), String>,
}

impl Ledger {
    /// Parses a ledger file leniently: a `#` line, a blank line, or a
    /// malformed row is skipped rather than failing the run, matching the
    /// `.pushed` loader. A torn final line from an interrupted write is
    /// therefore dropped, and its message read as not yet ingested -- the
    /// safe direction.
    pub(crate) fn parse(contents: &str) -> Ledger {
        let mut ledger = Ledger::default();
        for line in contents.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [mailbox, uidvalidity, uid, ingested_at] = fields[..] else {
                continue;
            };
            let (Ok(uidvalidity), Ok(uid)) = (uidvalidity.parse(), uid.parse()) else {
                continue;
            };
            ledger.insert(LedgerEntry {
                mailbox: mailbox.to_string(),
                uidvalidity,
                uid,
                ingested_at: ingested_at.to_string(),
            });
        }
        ledger
    }

    pub(crate) fn serialize(&self) -> String {
        let mut out = String::from(LEDGER_HEADER);
        out.push('\n');
        for ((mailbox, uidvalidity, uid), ingested_at) in &self.entries {
            out.push_str(&format!("{mailbox}\t{uidvalidity}\t{uid}\t{ingested_at}\n"));
        }
        out
    }

    /// Adds `entry`, keeping the **earlier** `ingested_at` when the key is
    /// already present: the ledger answers "when was this first ingested?".
    pub(crate) fn insert(&mut self, entry: LedgerEntry) {
        let key = (entry.mailbox, entry.uidvalidity, entry.uid);
        match self.entries.get(&key) {
            Some(existing) if existing.as_str() <= entry.ingested_at.as_str() => {}
            _ => {
                self.entries.insert(key, entry.ingested_at);
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The UIDs ingested from `mailbox` under its `uidvalidity` generation.
    /// Rows from an older generation are deliberately not returned: after a
    /// `UIDVALIDITY` reset every UID is renumbered, so they no longer name
    /// the messages they did.
    pub(crate) fn ingested_uids(&self, mailbox: &str, uidvalidity: u32) -> HashSet<u32> {
        self.entries
            .keys()
            .filter(|(m, v, _)| m == mailbox && *v == uidvalidity)
            .map(|(_, _, uid)| *uid)
            .collect()
    }
}

/// One server mailbox as the seed needs to know it.
pub(crate) struct SeedMailbox {
    pub mailbox: String,
    /// `sink::sanitize_mailbox_path`'s result, `/`-joined, no trailing `/`.
    pub relpath_key: String,
    pub uidvalidity: u32,
}

/// Builds a first ledger from what `email import` already delivered
/// (CLI-0164 §4). `listing` is a recursive listing of
/// `<destination>/<sanitized-email>/`, so each `Path` is
/// `<mailbox-relpath>/<uid>.eml`; anything else (attachments, foreign
/// objects) is ignored.
///
/// Every row is stamped with the mailbox's **current** `UIDVALIDITY`, the
/// only one knowable from here -- if it changed since the original import,
/// the seed is wrong in a way the listing cannot reveal (stated in the
/// ADR). A relpath that two server mailboxes sanitize to is ambiguous, so
/// its rows are skipped and returned: those messages then read as new
/// rather than being silently marked ingested under the wrong mailbox.
pub(crate) fn seed_from_listing(
    listing: &[LsjsonEntry],
    mailboxes: &[SeedMailbox],
    ingested_at: &str,
) -> (Ledger, Vec<String>) {
    let mut by_relpath: HashMap<&str, Vec<&SeedMailbox>> = HashMap::new();
    for mailbox in mailboxes {
        by_relpath
            .entry(mailbox.relpath_key.as_str())
            .or_default()
            .push(mailbox);
    }

    let mut ledger = Ledger::default();
    let mut ambiguous: Vec<String> = by_relpath
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(relpath, _)| relpath.to_string())
        .collect();
    ambiguous.sort();

    for entry in listing {
        let (dir, file) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
        let Some(uid) = file
            .strip_suffix(".eml")
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(owners) = by_relpath.get(dir) else {
            continue;
        };
        let [owner] = owners[..] else {
            continue;
        };
        ledger.insert(LedgerEntry {
            mailbox: owner.mailbox.clone(),
            uidvalidity: owner.uidvalidity,
            uid,
            ingested_at: ingested_at.to_string(),
        });
    }
    (ledger, ambiguous)
}

/// `relative_key`'s shape without the trailing `/`: a mailbox relpath as it
/// appears inside an object key.
pub(crate) fn relpath_key(mailbox_relpath: &Path) -> String {
    mailbox_relpath
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `YYYY-MM-DDTHH:MM:SSZ`, hand-formatted because `time`'s `formatting`
/// feature was pruned as unused (CLI-0148). Lexicographic order is
/// chronological order, which `Ledger::insert` relies on.
pub(crate) fn utc_timestamp(now: time::OffsetDateTime) -> String {
    let now = now.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

pub(crate) fn now_timestamp() -> String {
    utc_timestamp(time::OffsetDateTime::now_utc())
}

/// `<ledger-address><sanitized-email>/` -- one identity's state directory.
pub(crate) fn identity_dir_address(target: &RemoteTarget, email: &str) -> String {
    format!("{}{}/", target.address(), identity::sanitize_segment(email))
}

/// Downloads `<dir_address><name>` to `local_path` and returns its
/// contents, or `None` when the object does not exist.
///
/// Existence is decided by a listing rather than by reading a failed
/// `copyto`'s stderr: CLI-0140's `missing_ok` already tells "nothing there"
/// from "could not reach it", whereas matching rclone's error text is the
/// fragility issue #63 records. Any other failure is an `Err`, so an
/// unreachable ledger bucket is never mistaken for an empty ledger -- which
/// would re-ingest every message.
pub(crate) async fn download_state(
    dir_address: &str,
    name: &str,
    local_path: &Path,
    log_dir: &Path,
    job: &'static str,
) -> Result<Option<String>, String> {
    let listing = Lsjson {
        path: dir_address.to_string(),
        missing_ok: true,
        job,
        ..Default::default()
    }
    .run()
    .await?;
    if !listing.iter().any(|entry| entry.name == name) {
        return Ok(None);
    }

    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::create_dir_all(log_dir)
        .map_err(|err| format!("failed to create {}: {err}", log_dir.display()))?;
    CopyTo {
        source: format!("{dir_address}{name}"),
        destination: local_path.display().to_string(),
        log_path: log_dir.join(format!("{name}-pull.jsonl")),
        job,
        step: "state-pull",
        emit_upload_metrics: false,
        bytes: 0,
    }
    .run()
    .await?;
    fs::read_to_string(local_path)
        .map(Some)
        .map_err(|err| format!("failed to read {}: {err}", local_path.display()))
}

/// Uploads `local_path` to `<dir_address><name>`.
pub(crate) async fn upload_state(
    local_path: &Path,
    dir_address: &str,
    name: &str,
    log_dir: &Path,
    job: &'static str,
) -> Result<(), String> {
    fs::create_dir_all(log_dir)
        .map_err(|err| format!("failed to create {}: {err}", log_dir.display()))?;
    CopyTo {
        source: local_path.display().to_string(),
        destination: format!("{dir_address}{name}"),
        log_path: log_dir.join(format!("{name}-push.jsonl")),
        job,
        step: "state-push",
        emit_upload_metrics: false,
        bytes: 0,
    }
    .run()
    .await
}

/// Loads an identity's ledger, seeding it from `destination` when it does
/// not exist yet (CLI-0164 §4). A freshly seeded ledger is uploaded
/// immediately, so a concurrent `email scan` sees it. Returns the ledger
/// and whether it was seeded.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_or_seed(
    ledger_target: &RemoteTarget,
    destination: Option<&RemoteTarget>,
    alias: &str,
    email: &str,
    mailboxes: &[SeedMailbox],
    local_path: &Path,
    log_dir: &Path,
    job: &'static str,
) -> Result<(Ledger, bool), String> {
    let dir_address = identity_dir_address(ledger_target, email);
    if let Some(contents) =
        download_state(&dir_address, LEDGER_FILE, local_path, log_dir, job).await?
    {
        let ledger = Ledger::parse(&contents);
        crate::observability::metrics::record_phase(job, "ledger", "loaded", None);
        tracing::info!(identity = alias, rows = ledger.len(), "ledger: loaded");
        return Ok((ledger, false));
    }

    let Some(destination) = destination else {
        return Err(format!(
            "no ledger for '{alias}' at {dir_address}{LEDGER_FILE}; --destination is required to seed it"
        ));
    };
    let listing_address = format!(
        "{}{}/",
        destination.address(),
        identity::sanitize_segment(email)
    );
    // A never-imported account has no directory at all, which is a valid,
    // empty seed rather than an error.
    let listing = Lsjson {
        path: listing_address.clone(),
        recursive: true,
        include_extensions: vec!["eml".to_string()],
        missing_ok: true,
        job,
        ..Default::default()
    }
    .run()
    .await?;
    let (ledger, ambiguous) = seed_from_listing(&listing, mailboxes, &now_timestamp());
    for relpath in &ambiguous {
        tracing::warn!(
            identity = alias,
            relpath = %relpath,
            "ledger seed: more than one mailbox sanitizes to this path; its messages are left unseeded and will read as new"
        );
    }
    tracing::info!(
        identity = alias,
        source = %listing_address,
        listed = listing.len(),
        rows = ledger.len(),
        ambiguous = ambiguous.len(),
        "ledger: seeded from destination"
    );

    write_local(&ledger, local_path)?;
    upload_state(local_path, &dir_address, LEDGER_FILE, log_dir, job).await?;
    crate::observability::metrics::record_phase(job, "ledger", "seeded", None);
    Ok((ledger, true))
}

/// Writes `ledger` to `local_path` whole, replacing whatever is there.
pub(crate) fn write_local(ledger: &Ledger, local_path: &Path) -> Result<(), String> {
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(local_path, ledger.serialize())
        .map_err(|err| format!("failed to write {}: {err}", local_path.display()))
}

/// `email import`'s live handle on one identity's ledger: appends each
/// delivery to a local working copy and periodically pushes it whole.
///
/// The local copy starts as the downloaded (or seeded) ledger, so every
/// push carries the full history, not just this run's rows. Appends and the
/// pre-push snapshot share one lock, so an upload never reads a half-written
/// row.
pub(crate) struct LedgerSync {
    pub dir_address: String,
    pub local_path: PathBuf,
    pub log_dir: PathBuf,
    append_lock: Mutex<()>,
}

impl LedgerSync {
    pub(crate) fn new(dir_address: String, local_path: PathBuf, log_dir: PathBuf) -> LedgerSync {
        LedgerSync {
            dir_address,
            local_path,
            log_dir,
            append_lock: Mutex::new(()),
        }
    }

    pub(crate) fn append(&self, entry: &LedgerEntry) -> Result<(), String> {
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.local_path)
            .map_err(|err| format!("failed to open {}: {err}", self.local_path.display()))?;
        writeln!(
            file,
            "{}\t{}\t{}\t{}",
            entry.mailbox, entry.uidvalidity, entry.uid, entry.ingested_at
        )
        .map_err(|err| format!("failed to write {}: {err}", self.local_path.display()))
    }

    /// Pushes the working copy, via a snapshot taken under the append lock.
    pub(crate) async fn flush(&self, job: &'static str) -> Result<(), String> {
        let snapshot = self.local_path.with_extension("tsv.snapshot");
        {
            let _guard = self
                .append_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            fs::copy(&self.local_path, &snapshot).map_err(|err| {
                format!(
                    "failed to snapshot {} to {}: {err}",
                    self.local_path.display(),
                    snapshot.display()
                )
            })?;
        }
        let result = upload_state(
            &snapshot,
            &self.dir_address,
            LEDGER_FILE,
            &self.log_dir,
            job,
        )
        .await;
        let _ = fs::remove_file(&snapshot);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mailbox: &str, uidvalidity: u32, uid: u32, at: &str) -> LedgerEntry {
        LedgerEntry {
            mailbox: mailbox.to_string(),
            uidvalidity,
            uid,
            ingested_at: at.to_string(),
        }
    }

    fn listed(path: &str) -> LsjsonEntry {
        LsjsonEntry {
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            size: 1,
        }
    }

    fn seed_mailbox(mailbox: &str, relpath: &str, uidvalidity: u32) -> SeedMailbox {
        SeedMailbox {
            mailbox: mailbox.to_string(),
            relpath_key: relpath.to_string(),
            uidvalidity,
        }
    }

    #[test]
    fn parse_and_serialize_round_trip() {
        let mut ledger = Ledger::default();
        ledger.insert(entry("INBOX", 7, 2, "2026-10-20T00:00:00Z"));
        ledger.insert(entry("Archive/2020", 9, 1, "2026-10-19T00:00:00Z"));

        let text = ledger.serialize();

        assert!(text.starts_with(LEDGER_HEADER));
        assert_eq!(Ledger::parse(&text), ledger);
    }

    #[test]
    fn parse_skips_headers_blank_and_malformed_lines() {
        let ledger = Ledger::parse(
            "# header\n\nINBOX\t7\t1\t2026-10-20T00:00:00Z\nINBOX\t7\t1\nINBOX\tx\t2\tt\nINBOX\t7\t3\t2026-10-20T00:00:00Z\n",
        );

        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.ingested_uids("INBOX", 7), HashSet::from([1, 3]));
    }

    #[test]
    fn insert_keeps_the_earliest_ingested_at() {
        let mut ledger = Ledger::default();
        ledger.insert(entry("INBOX", 7, 1, "2026-10-20T00:00:00Z"));
        ledger.insert(entry("INBOX", 7, 1, "2026-10-21T00:00:00Z"));
        ledger.insert(entry("INBOX", 7, 1, "2026-10-19T00:00:00Z"));

        assert_eq!(ledger.len(), 1);
        assert!(ledger.serialize().contains("2026-10-19T00:00:00Z"));
    }

    #[test]
    fn ingested_uids_ignores_other_mailboxes_and_older_uidvalidity() {
        let mut ledger = Ledger::default();
        ledger.insert(entry("INBOX", 7, 1, "t"));
        ledger.insert(entry("INBOX", 6, 2, "t"));
        ledger.insert(entry("Archive", 7, 3, "t"));

        assert_eq!(ledger.ingested_uids("INBOX", 7), HashSet::from([1]));
        assert!(ledger.ingested_uids("INBOX", 8).is_empty());
    }

    #[test]
    fn seed_maps_eml_keys_back_to_their_mailbox() {
        let listing = vec![
            listed("inbox/1.eml"),
            listed("inbox/2.eml"),
            listed("archive-2020/5.eml"),
        ];
        let mailboxes = vec![
            seed_mailbox("INBOX", "inbox", 7),
            seed_mailbox("Archive/2020", "archive-2020", 9),
        ];

        let (ledger, ambiguous) = seed_from_listing(&listing, &mailboxes, "now");

        assert!(ambiguous.is_empty());
        assert_eq!(ledger.ingested_uids("INBOX", 7), HashSet::from([1, 2]));
        assert_eq!(ledger.ingested_uids("Archive/2020", 9), HashSet::from([5]));
    }

    #[test]
    fn seed_ignores_attachments_and_foreign_objects() {
        let listing = vec![
            listed("inbox/1/attachments/invoice.eml"),
            listed("inbox/notes.eml"),
            listed("unknown/3.eml"),
            listed("inbox/4.eml"),
        ];
        let mailboxes = vec![seed_mailbox("INBOX", "inbox", 7)];

        let (ledger, _) = seed_from_listing(&listing, &mailboxes, "now");

        assert_eq!(ledger.ingested_uids("INBOX", 7), HashSet::from([4]));
    }

    #[test]
    fn seed_skips_a_relpath_two_mailboxes_share() {
        let listing = vec![listed("a-b/1.eml"), listed("c/2.eml")];
        let mailboxes = vec![
            seed_mailbox("A/B", "a-b", 1),
            seed_mailbox("A.B", "a-b", 2),
            seed_mailbox("C", "c", 3),
        ];

        let (ledger, ambiguous) = seed_from_listing(&listing, &mailboxes, "now");

        assert_eq!(ambiguous, vec!["a-b".to_string()]);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.ingested_uids("C", 3), HashSet::from([2]));
    }

    #[test]
    fn seed_handles_a_mailbox_at_the_identity_root() {
        let (ledger, _) = seed_from_listing(&[listed("9.eml")], &[seed_mailbox("X", "", 1)], "now");

        assert_eq!(ledger.ingested_uids("X", 1), HashSet::from([9]));
    }

    #[test]
    fn utc_timestamp_is_zero_padded_and_sorts_chronologically() {
        let early = time::OffsetDateTime::from_unix_timestamp(1_767_323_045).unwrap();
        let late = time::OffsetDateTime::from_unix_timestamp(1_792_501_200).unwrap();

        assert_eq!(utc_timestamp(early), "2026-01-02T03:04:05Z");
        assert!(utc_timestamp(early) < utc_timestamp(late));
    }

    #[test]
    fn ledger_sync_appends_onto_the_seeded_copy_and_parses_back() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(LEDGER_FILE);
        let mut seeded = Ledger::default();
        seeded.insert(entry("INBOX", 7, 1, "2026-10-19T00:00:00Z"));
        write_local(&seeded, &local).unwrap();

        let sync = LedgerSync::new("unused:".into(), local.clone(), dir.path().into());
        sync.append(&entry("INBOX", 7, 2, "2026-10-20T00:00:00Z"))
            .unwrap();

        let reread = Ledger::parse(&fs::read_to_string(&local).unwrap());
        assert_eq!(reread.ingested_uids("INBOX", 7), HashSet::from([1, 2]));
    }

    /// `rclone` works against plain local paths, so the state round trip
    /// is testable without a bucket. Skipped (not failed) when `rclone` is
    /// absent, following `push.rs`'s existing test.
    #[tokio::test]
    async fn state_round_trips_and_a_missing_object_reads_as_none() {
        if crate::core::rclone::check_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let remote_dir = dir.path().join("remote");
        fs::create_dir_all(&remote_dir).unwrap();
        let dir_address = format!("{}/", remote_dir.display());
        let logs = dir.path().join("logs");

        let missing = download_state(
            &dir_address,
            LEDGER_FILE,
            &dir.path().join("pulled.tsv"),
            &logs,
            "test",
        )
        .await
        .unwrap();
        assert!(missing.is_none());

        let local = dir.path().join("local.tsv");
        fs::write(&local, "INBOX\t7\t1\tt\n").unwrap();
        upload_state(&local, &dir_address, LEDGER_FILE, &logs, "test")
            .await
            .unwrap();

        let pulled = download_state(
            &dir_address,
            LEDGER_FILE,
            &dir.path().join("pulled.tsv"),
            &logs,
            "test",
        )
        .await
        .unwrap();
        assert_eq!(pulled.as_deref(), Some("INBOX\t7\t1\tt\n"));
    }
}

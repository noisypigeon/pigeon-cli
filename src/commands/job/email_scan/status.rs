//! `scan-status.tsv` (CLI-0164 §6): the latest scan of one identity, one
//! row per mailbox plus a `*` total row, overwritten by every scan. It is
//! the file a downstream scheduler reads to decide whether an import batch
//! is worth running; the full history of scans lives in each run's
//! `--debug-log-bucket` report instead.
//!
//! Per row, `server_messages = ingested - removed_on_server + new_messages`
//! always holds: `ingested` counts ledger rows for the mailbox's current
//! `UIDVALIDITY`, `new_messages` what the server has that the ledger lacks,
//! and `removed_on_server` what the ledger has that the server no longer
//! does (informational -- the ledger never forgets a row, CLI-0164 §3).

use std::collections::{HashMap, HashSet};

/// The status object's name inside an identity's ledger directory.
pub(crate) const STATUS_FILE: &str = "scan-status.tsv";

/// The total row's `mailbox` value.
pub(crate) const TOTAL_ROW: &str = "*";

const HEADER: &str = "# mailbox\tuidvalidity\tserver_messages\tingested\tnew_messages\tnew_attachments\tremoved_on_server\tlast_scanned_at\tlast_in_sync_at";

/// One row of `scan-status.tsv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxStatus {
    pub mailbox: String,
    /// `None` on the total row.
    pub uidvalidity: Option<u32>,
    pub server_messages: usize,
    pub ingested: usize,
    pub new_messages: usize,
    /// `None` when not counted: over `ATTACHMENT_COUNT_CAP`, or the count
    /// itself failed. Written as `unknown`.
    pub new_attachments: Option<u64>,
    pub removed_on_server: usize,
    pub last_scanned_at: String,
    /// The last scan that found nothing new -- "when was this mailbox last
    /// confirmed fully ingested?". `None` (written `-`) if never.
    pub last_in_sync_at: Option<String>,
}

/// Diffs one mailbox's server UIDs against its ingested UIDs.
pub(crate) fn diff(
    mailbox: &str,
    uidvalidity: u32,
    server: &HashSet<u32>,
    ingested: &HashSet<u32>,
    scanned_at: &str,
) -> (MailboxStatus, Vec<u32>) {
    let mut new_uids: Vec<u32> = server.difference(ingested).copied().collect();
    new_uids.sort_unstable();
    let status = MailboxStatus {
        mailbox: mailbox.to_string(),
        uidvalidity: Some(uidvalidity),
        server_messages: server.len(),
        ingested: ingested.len(),
        new_messages: new_uids.len(),
        new_attachments: Some(0),
        removed_on_server: ingested.difference(server).count(),
        last_scanned_at: scanned_at.to_string(),
        last_in_sync_at: None,
    };
    (status, new_uids)
}

/// Sums `rows` into the `*` row. Attachments are `unknown` if any
/// mailbox's are.
pub(crate) fn total(rows: &[MailboxStatus], scanned_at: &str) -> MailboxStatus {
    MailboxStatus {
        mailbox: TOTAL_ROW.to_string(),
        uidvalidity: None,
        server_messages: rows.iter().map(|row| row.server_messages).sum(),
        ingested: rows.iter().map(|row| row.ingested).sum(),
        new_messages: rows.iter().map(|row| row.new_messages).sum(),
        new_attachments: rows
            .iter()
            .map(|row| row.new_attachments)
            .sum::<Option<u64>>(),
        removed_on_server: rows.iter().map(|row| row.removed_on_server).sum(),
        last_scanned_at: scanned_at.to_string(),
        last_in_sync_at: None,
    }
}

/// Sets each row's `last_in_sync_at`: now if it has nothing new, otherwise
/// whatever the previous snapshot recorded for the same mailbox (or `None`).
/// Applies to the total row the same way, so `*` answers "when was the
/// whole account last confirmed fully ingested?".
pub(crate) fn carry_forward(previous: &[MailboxStatus], current: &mut [MailboxStatus]) {
    let previous: HashMap<&str, &Option<String>> = previous
        .iter()
        .map(|row| (row.mailbox.as_str(), &row.last_in_sync_at))
        .collect();
    for row in current {
        row.last_in_sync_at = if row.new_messages == 0 {
            Some(row.last_scanned_at.clone())
        } else {
            previous
                .get(row.mailbox.as_str())
                .and_then(|value| (*value).clone())
        };
    }
}

pub(crate) fn serialize(rows: &[MailboxStatus]) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for row in rows {
        out.push_str(&format_row(row));
        out.push('\n');
    }
    out
}

/// One row, tab-separated, in `HEADER`'s column order.
pub(crate) fn format_row(row: &MailboxStatus) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        row.mailbox,
        row.uidvalidity
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        row.server_messages,
        row.ingested,
        row.new_messages,
        row.new_attachments
            .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
        row.removed_on_server,
        row.last_scanned_at,
        row.last_in_sync_at.as_deref().unwrap_or("-"),
    )
}

/// Parses a previous snapshot leniently; only `last_in_sync_at` is ever
/// read back, but every column is parsed so a malformed row is dropped
/// whole rather than half-trusted.
pub(crate) fn parse(contents: &str) -> Vec<MailboxStatus> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [
                mailbox,
                uidvalidity,
                server,
                ingested,
                new,
                attachments,
                removed,
                scanned,
                in_sync,
            ] = fields[..]
            else {
                return None;
            };
            Some(MailboxStatus {
                mailbox: mailbox.to_string(),
                uidvalidity: uidvalidity.parse().ok(),
                server_messages: server.parse().ok()?,
                ingested: ingested.parse().ok()?,
                new_messages: new.parse().ok()?,
                new_attachments: attachments.parse().ok(),
                removed_on_server: removed.parse().ok()?,
                last_scanned_at: scanned.to_string(),
                last_in_sync_at: (in_sync != "-").then(|| in_sync.to_string()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_reconciles_server_ingested_new_and_removed() {
        let server = HashSet::from([1, 2, 3, 5]);
        let ingested = HashSet::from([1, 2, 4]);

        let (row, new_uids) = diff("INBOX", 7, &server, &ingested, "t");

        assert_eq!(new_uids, vec![3, 5]);
        assert_eq!(row.server_messages, 4);
        assert_eq!(row.ingested, 3);
        assert_eq!(row.new_messages, 2);
        assert_eq!(row.removed_on_server, 1);
        assert_eq!(
            row.server_messages,
            row.ingested - row.removed_on_server + row.new_messages
        );
    }

    #[test]
    fn total_sums_rows_and_any_unknown_attachment_count_is_unknown() {
        let (mut a, _) = diff("A", 1, &HashSet::from([1, 2]), &HashSet::from([1]), "t");
        a.new_attachments = Some(3);
        let (mut b, _) = diff("B", 1, &HashSet::from([1]), &HashSet::new(), "t");
        b.new_attachments = Some(2);

        let sum = total(&[a.clone(), b.clone()], "t");
        assert_eq!(sum.mailbox, TOTAL_ROW);
        assert_eq!((sum.server_messages, sum.new_messages), (3, 2));
        assert_eq!(sum.new_attachments, Some(5));

        b.new_attachments = None;
        assert_eq!(total(&[a, b], "t").new_attachments, None);
    }

    #[test]
    fn carry_forward_stamps_in_sync_rows_and_keeps_the_old_value_otherwise() {
        let previous = vec![MailboxStatus {
            last_in_sync_at: Some("2026-10-19T00:00:00Z".into()),
            ..diff("INBOX", 7, &HashSet::new(), &HashSet::new(), "old").0
        }];
        let (busy, _) = diff("INBOX", 7, &HashSet::from([1]), &HashSet::new(), "now");
        let (quiet, _) = diff("Sent", 7, &HashSet::from([1]), &HashSet::from([1]), "now");
        let (fresh, _) = diff("New", 7, &HashSet::from([1]), &HashSet::new(), "now");
        let mut current = vec![busy, quiet, fresh];

        carry_forward(&previous, &mut current);

        assert_eq!(
            current[0].last_in_sync_at.as_deref(),
            Some("2026-10-19T00:00:00Z")
        );
        assert_eq!(current[1].last_in_sync_at.as_deref(), Some("now"));
        assert_eq!(current[2].last_in_sync_at, None);
    }

    #[test]
    fn serialize_and_parse_round_trip_including_the_total_row() {
        let (mut row, _) = diff("Archive/2020", 9, &HashSet::from([1]), &HashSet::new(), "t");
        row.new_attachments = None;
        let mut rows = vec![row.clone()];
        rows.push(total(&rows, "t"));
        carry_forward(&[], &mut rows);

        let text = serialize(&rows);

        assert!(text.starts_with(HEADER));
        assert!(text.contains("unknown"));
        assert_eq!(parse(&text), rows);
    }

    #[test]
    fn parse_drops_malformed_rows() {
        assert!(
            parse("# header\nINBOX\t7\tnot-a-number\t0\t0\t0\t0\tt\t-\nshort\trow\n").is_empty()
        );
    }
}

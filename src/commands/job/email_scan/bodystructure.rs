//! Counting attachments on new messages without fetching them (CLI-0164
//! §5): one `UID FETCH <set> (UID BODYSTRUCTURE)` per batch of **new** UIDs
//! only -- MIME structure metadata, never a body.
//!
//! Brought back from `email_sync::manifest` (removed by CLI-0147), trimmed
//! to its counting half: CLI-0032/0033's heuristic and CLI-0065's bisection.
//! CLI-0146 §7 refused this pass for `email import` because there it would
//! cover the *whole* pending corpus at a throttled pace; here it covers the
//! delta since the last ingest, and is capped (`ATTACHMENT_COUNT_CAP`) for
//! the first scan of an account that has never been imported, where the
//! delta is everything.

use async_imap::imap_proto::types::BodyStructure;
use futures::TryStreamExt;

use crate::commands::keyring::email::imap_client::ImapSession;

/// Above this many new UIDs in one identity, attachments are not counted
/// at all and the status reads `unknown`: a first scan of a never-imported
/// account would otherwise traverse the whole mailbox, the exact cost
/// CLI-0146 §7 declined.
pub(crate) const ATTACHMENT_COUNT_CAP: usize = 5_000;

/// UIDs per `BODYSTRUCTURE` request.
const FETCH_BATCH: usize = 50;

/// The attachment estimate for one mailbox's new UIDs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttachmentCount {
    pub attachments: u64,
    /// UIDs whose `BODYSTRUCTURE` imap-proto could not parse (CLI-0065),
    /// counted as zero attachments.
    pub unparsed: usize,
}

/// Whether attachments should be counted for an identity with
/// `total_new` new UIDs.
pub(crate) fn within_cap(total_new: usize) -> bool {
    total_new <= ATTACHMENT_COUNT_CAP
}

/// Counts attachment leaf parts in a `BODYSTRUCTURE` tree: a non-multipart
/// part counts if its `Content-Disposition` is `attachment`
/// (case-insensitive) *or* its `Content-Type` carries a `name` param, since
/// some MUAs never set a disposition at all (CLI-0032, widened by CLI-0033).
/// An estimate: `email import`'s `mail_parser` extraction is the
/// authoritative count and is not guaranteed to agree.
fn count_attachments(structure: &BodyStructure) -> u64 {
    match structure {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().map(count_attachments).sum(),
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. } => {
            let has_attachment_disposition = common
                .disposition
                .as_ref()
                .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
            let has_content_type_name = common.ty.params.as_ref().is_some_and(|params| {
                params
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("name"))
            });
            u64::from(has_attachment_disposition || has_content_type_name)
        }
    }
}

async fn fetch_batch(
    session: &mut ImapSession,
    mailbox: &str,
    uids: &[u32],
) -> Result<u64, String> {
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut fetches = session
        .uid_fetch(&uid_set, "(UID BODYSTRUCTURE)")
        .await
        .map_err(|err| format!("failed to fetch BODYSTRUCTURE in '{mailbox}': {err}"))?;
    let mut attachments = 0;
    while let Some(fetch) = fetches
        .try_next()
        .await
        .map_err(|err| format!("failed to fetch BODYSTRUCTURE in '{mailbox}': {err}"))?
    {
        attachments += fetch.bodystructure().map_or(0, count_attachments);
    }
    Ok(attachments)
}

/// What to do after one batch attempt -- pure, so CLI-0065's bisection is
/// testable without an IMAP session.
#[derive(Debug, PartialEq, Eq)]
enum BisectOutcome {
    Fetched(u64),
    /// A single UID that still failed alone: confirmed unparseable.
    Poisoned,
    Retry(Vec<u32>, Vec<u32>),
}

/// One unparseable `BODYSTRUCTURE` (a bounce's `MESSAGE/DELIVERY-STATUS`,
/// an internationalized `MESSAGE/GLOBAL`) fails the *entire* batched
/// response in imap-proto, so a failed batch is split until the poisoned
/// UID is isolated, rather than losing every other UID's count with it.
fn bisect_step(batch: &[u32], result: Result<u64, String>) -> BisectOutcome {
    match result {
        Ok(attachments) => BisectOutcome::Fetched(attachments),
        Err(_) if batch.len() <= 1 => BisectOutcome::Poisoned,
        Err(_) => {
            let mid = batch.len() / 2;
            BisectOutcome::Retry(batch[..mid].to_vec(), batch[mid..].to_vec())
        }
    }
}

/// Counts attachments across `uids` in `mailbox`, which must already be
/// selected (`EXAMINE`d) on `session`.
pub(crate) async fn count_new_attachments(
    session: &mut ImapSession,
    mailbox: &str,
    uids: &[u32],
) -> AttachmentCount {
    let mut count = AttachmentCount::default();
    let mut work: Vec<Vec<u32>> = uids
        .chunks(FETCH_BATCH)
        .map(<[u32]>::to_vec)
        .rev()
        .collect();
    while let Some(batch) = work.pop() {
        let result = fetch_batch(session, mailbox, &batch).await;
        match bisect_step(&batch, result) {
            BisectOutcome::Fetched(attachments) => count.attachments += attachments,
            BisectOutcome::Poisoned => {
                tracing::warn!(
                    mailbox,
                    uid = batch.first().copied(),
                    "BODYSTRUCTURE could not be parsed; counting no attachments for this message"
                );
                count.unparsed += batch.len();
            }
            BisectOutcome::Retry(left, right) => {
                work.push(right);
                work.push(left);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use async_imap::imap_proto::types::{
        BodyContentCommon, BodyContentSinglePart, ContentDisposition, ContentEncoding, ContentType,
    };

    use super::*;

    fn basic_part(
        disposition: Option<&'static str>,
        name: Option<&'static str>,
    ) -> BodyStructure<'static> {
        BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "application".into(),
                    subtype: "octet-stream".into(),
                    params: name.map(|name| vec![("name".into(), name.into())]),
                },
                disposition: disposition.map(|ty| ContentDisposition {
                    ty: ty.into(),
                    params: None,
                }),
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::SevenBit,
                octets: 100,
            },
            extension: None,
        }
    }

    fn multipart(bodies: Vec<BodyStructure<'static>>) -> BodyStructure<'static> {
        BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "multipart".into(),
                    subtype: "mixed".into(),
                    params: None,
                },
                disposition: None,
                language: None,
                location: None,
            },
            bodies,
            extension: None,
        }
    }

    #[test]
    fn a_bare_part_is_not_an_attachment() {
        assert_eq!(count_attachments(&basic_part(None, None)), 0);
        assert_eq!(count_attachments(&basic_part(Some("inline"), None)), 0);
    }

    #[test]
    fn a_name_param_counts_without_a_disposition() {
        assert_eq!(count_attachments(&basic_part(None, Some("a.pdf"))), 1);
    }

    #[test]
    fn nested_multiparts_sum_their_attachments() {
        let nested = multipart(vec![
            basic_part(None, None),
            basic_part(Some("Attachment"), None),
            basic_part(Some("attachment"), None),
        ]);
        let top = multipart(vec![basic_part(Some("inline"), None), nested]);

        assert_eq!(count_attachments(&top), 2);
    }

    #[test]
    fn bisect_splits_a_failed_batch_and_poisons_a_lone_uid() {
        assert_eq!(bisect_step(&[1, 2, 3], Ok(4)), BisectOutcome::Fetched(4));
        assert_eq!(
            bisect_step(&[1, 2, 3], Err("parse".into())),
            BisectOutcome::Retry(vec![1], vec![2, 3])
        );
        assert_eq!(
            bisect_step(&[9], Err("parse".into())),
            BisectOutcome::Poisoned
        );
    }

    #[test]
    fn bisect_converges_on_the_one_poisoned_uid() {
        let fake = |batch: &[u32]| -> Result<u64, String> {
            if batch.contains(&3) {
                Err("parse".into())
            } else {
                Ok(batch.len() as u64)
            }
        };
        let mut work = vec![vec![1u32, 2, 3, 4, 5]];
        let (mut fetched, mut poisoned) = (0, 0);
        while let Some(batch) = work.pop() {
            match bisect_step(&batch, fake(&batch)) {
                BisectOutcome::Fetched(n) => fetched += n,
                BisectOutcome::Poisoned => poisoned += 1,
                BisectOutcome::Retry(left, right) => {
                    work.push(right);
                    work.push(left);
                }
            }
        }

        assert_eq!((fetched, poisoned), (4, 1));
    }

    #[test]
    fn the_cap_is_pinned_and_inclusive() {
        assert_eq!(ATTACHMENT_COUNT_CAP, 5_000);
        assert!(within_cap(ATTACHMENT_COUNT_CAP));
        assert!(!within_cap(ATTACHMENT_COUNT_CAP + 1));
    }
}

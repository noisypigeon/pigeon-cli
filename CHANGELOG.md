# Changelog

One line per PR across this whole repo, sectioned by date, newest first.
Not versioned — for versioned, package-scoped changelogs see
[`service/pigeon-cli/CHANGELOG.md`](service/pigeon-cli/CHANGELOG.md) (the
`pigeon-cli` crate) and `terraform/modules/*/*/CHANGELOG.md` (each
Terraform module). Entry format: `- [<scope>] <summary> ([#N](PR URL))`,
where `<scope>` is `pigeon-cli`, `blog`, `terraform/<provider>/<module>`, or
`repo` for cross-cutting/structural changes. Starts fresh at ADR-0050 — no
backfill of prior history.

## 2026-09-30

- [pigeon-cli] fix(adr-0085): add Linux keyring support via kernel keyutils ([#2](https://github.com/noisypigeon/pigeon-cli/pull/2))
- [repo] docs(adr-0084): split pigeon-cli into its own repo ([#99](https://github.com/noisypigeon/noisypigeon/pull/99))
- [pigeon-cli] feat(adr-0083): implement sort job ([#98](https://github.com/noisypigeon/noisypigeon/pull/98))
- [pigeon-cli] docs(adr-0083): add sort job ADR ([#96](https://github.com/noisypigeon/noisypigeon-2/pull/96))
- [pigeon-cli] feat(adr-0082): implement dedupe job ([#95](https://github.com/noisypigeon/noisypigeon-2/pull/95))
- [pigeon-cli] docs(adr-0082): add dedupe job ADR ([#94](https://github.com/noisypigeon/noisypigeon/pull/94))
- [pigeon-cli] feat(adr-0081): implement email-pull job ([#93](https://github.com/noisypigeon/noisypigeon/pull/93))
- [pigeon-cli] docs(adr-0081): add email-pull job ADR ([#92](https://github.com/noisypigeon/noisypigeon/pull/92))

## 2026-09-29

- [terraform/digitalocean/droplet] Drop Cloudflare DNS record integration ([#91](https://github.com/noisypigeon/noisypigeon/pull/91))

- [terraform/digitalocean/droplet] Accept caller-supplied bucket credentials per rclone remote ([#90](https://github.com/noisypigeon/noisypigeon/pull/90))

- [terraform/scaleway/compute-instance] docs(adr-0079): add scaleway/compute-instance module ([#86](https://github.com/noisypigeon/noisypigeon/pull/86))

## 2026-09-28

- [pigeon-cli] feat(adr-0080): add a per-identity IMAP connection cap via keyring, and route transform lenient-skip warnings through tracing ([#87](https://github.com/noisypigeon/noisypigeon/pull/87))
- [repo] feat(adr-0078): formalize a job-run log analysis procedure and package it as the `analyze-job-run` Claude Code skill ([#85](https://github.com/noisypigeon/noisypigeon/pull/85))

## 2026-09-27

- [pigeon-cli] feat(adr-0077): add pull-transform file-type selection, adaptable transcoding mapping, and zip pass-through ([#84](https://github.com/noisypigeon/noisypigeon/pull/84))

- [pigeon-cli] fix(adr-0076): stream pull-transform downloads and zip expansion to disk instead of buffering whole objects/zips in memory, fixing a real SIGKILL crash against 50-100GB zip archives ([#83](https://github.com/noisypigeon/noisypigeon/pull/83))

- [pigeon-cli] feat(adr-0075): add progress visibility to pull-transform -- a live bar plus named call-outs for large downloads/recodes, so a long run no longer looks hung ([#82](https://github.com/noisypigeon/noisypigeon/pull/82))

- [pigeon-cli] feat(adr-0074): add pull-transform job -- pulls a bucket, expands zips, recodes media via ffmpeg with verify/fallback, dates and dedups by content, and organizes/uploads the result ([#81](https://github.com/noisypigeon/noisypigeon/pull/81))

- [pigeon-cli] feat(adr-0073): add cross-cutting observability -- structured tracing, a durable JSONL log, and CPU/mem/disk telemetry via one Observable trait reused by every command ([#80](https://github.com/noisypigeon/noisypigeon/pull/80))

- [terraform/scaleway/object-bucket] fix(adr-0072): raise GLACIER transition to Scaleway's 90-day minimum ([#77](https://github.com/noisypigeon/noisypigeon/pull/77))

- [terraform/scaleway/iam-policy] fix(adr-0070): downgrade admin bucket-policy statement to a supported version ([#73](https://github.com/noisypigeon/noisypigeon/pull/73))

- [terraform/scaleway/iam-policy] feat(adr-0069): guard scaleway/iam-policy against bucket-policy self-lockout ([#71](https://github.com/noisypigeon/pigeon/pull/71))

- [blog] ADR-0067: rewrite the noisypigeon.github.io blog from Jekyll to Zola as `service/blog` ([#66](https://github.com/noisypigeon/pigeon/pull/66))
- [pigeon-cli] ADR-0068: treat IMAP `LOGOUT` failures as best-effort, not fatal -- fixes a crash (and silent manifest-data loss) when the connection drops right after a successful `job run email-sync` mailbox scan ([#67](https://github.com/noisypigeon/pigeon/pull/67)).
- [pigeon-cli] ADR-0071: cap simultaneous IMAP connections per identity, retry a transiently-failed batch once, and collapse per-UID fetch-failure warning spam into one line per batch ([#76](https://github.com/noisypigeon/noisypigeon/pull/76))

- [terraform/scaleway/iam-policy] feat(adr-0066): guard scaleway/iam-policy against bucket-scope widening ([#64](https://github.com/noisypigeon/pigeon/pull/64))
- [pigeon-cli] ADR-0065: fix a `job run email-sync` crash caused by a single unparseable `BODYSTRUCTURE` message aborting an entire mailbox's manifest gathering -- `pull_manifest` now bisects the UID batch to isolate just the poisoned message(s) ([#65](https://github.com/noisypigeon/pigeon/pull/65)).

## 2026-09-26

- [terraform/digitalocean/droplet] Fix droplet module's access-key dependency source ([#63](https://github.com/noisypigeon/pigeon/pull/63))

- [repo] ADR-0050: relocate `Cargo.toml`/`Cargo.lock` into `service/pigeon-cli/`, add this repo-wide dated changelog, rename `LICENSE` to `LICENSE.md`, and rewrite both READMEs ([#59](https://github.com/noisypigeon/pigeon/pull/59)).

## 2026-09-25

- [repo] ADR-0051: rename GitHub repo references `pigeon-cli` → `pigeon`, correct `Cargo.toml`'s `repository` field, and bump to `0.2.1` in prep for the next publish ([#60](https://github.com/noisypigeon/pigeon/pull/60)).
- [repo] ADR-0052: decide how to merge the separate `pigeon-do` repo's full history into this repo as `terraform/infrastructure/*`, a new sibling to `terraform/modules/` (documents the decision; the user performs the actual merge manually) ([#62](https://github.com/noisypigeon/pigeon/pull/62)).

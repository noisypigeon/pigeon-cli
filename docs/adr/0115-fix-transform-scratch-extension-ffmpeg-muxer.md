# ADR-0115: `transform` forces ffmpeg's `mjpeg` muxer instead of relying on the scratch path's extension

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

A real `pigeon job run transform --input-file-type png ...` run (ADR-0112)
reported `0 transcoded, 0 copied through, 14 failed` -- every PNG input
failed identically:

```
ffmpeg failed to transcode /mnt/data/a/source/<file>.png: [NULL @ ...]
Unable to find a suitable output format for
'/mnt/data/a/.staging/scratch/<file>-<hash>.jpg.scratch': Invalid argument
```

`transcode_to_jpg` (`src/commands/job/transform/media.rs`) invokes `ffmpeg`
with no `-f <format>` flag, so ffmpeg infers the output container/muxer
from the output path's final extension. `process_one`
(`src/commands/job/transform/worker.rs:97-100`) builds that scratch path as:

```rust
let scratch_path = scratch_dir.join(format!(
    "{}.scratch",
    placement::compute_destination_name(source_path, &pending_file.relative_path)
));
```

`compute_destination_name` already returns a name ending in `.jpg`
(`placement.rs:41`), so appending `.scratch` produces e.g.
`foo-<hash>.jpg.scratch` -- a final extension ffmpeg doesn't recognize, so
format auto-detection fails for every png/heic input. `jpeg` inputs are
unaffected because `copy_through` never invokes ffmpeg at all (a plain
`fs::copy`), which is why this only shows up transcoding png/heic.

Reproduced directly with a bare `ffmpeg` invocation against a
`*.jpg.scratch`-named output path, confirming the failure and that adding
`-f mjpeg` fixes it. This also explains why `media.rs`'s own unit tests
never caught it: they pass a plain `test.jpg` output path, never the real
`*.jpg.scratch` shape `worker.rs` actually constructs.

## Decision

Add `-f mjpeg` to `transcode_to_jpg`'s ffmpeg invocation
(`src/commands/job/transform/media.rs`), forcing the muxer explicitly so
transcoding no longer depends on the output filename's extension at all:

```
ffmpeg -y -loglevel error -i <input> -frames:v 1 -q:v 1 -pix_fmt yuvj444p -f mjpeg <output>
```

The scratch-naming scheme itself (`worker.rs`/`placement.rs`) is untouched
-- it's not wrong, ffmpeg's extension-based format sniffing is the actual
problem, and forcing the muxer is strictly more robust than trying to keep
every future scratch-path convention ending in a recognizable extension.

Added a regression test,
`transcode_to_jpg_succeeds_when_the_output_path_ends_in_scratch`, that
mirrors `worker.rs`'s real scratch-path shape (`<name>-<hash>.jpg.scratch`)
directly, so a future regression on this exact failure mode is caught by
`mise run test` rather than only in production.

## Consequences

- `job run transform` correctly transcodes png/heic inputs again; the bug
  affected every run of this job type since ADR-0112 shipped it.
- `copy_through` (jpeg inputs) was never affected and needs no change.
- No change to checkpointing, placement, or the CLI surface -- this is an
  internal `ffmpeg` invocation fix only.

## Out of scope

- `pull_transform`'s own ffmpeg transcoding path (`pull_transform/media.rs`)
  doesn't have this bug: its scratch files are named directly with their
  real extension via `next_scratch_path(dir, counter, extension)`, never a
  double-stacked `<name>.<ext>.scratch`. Nothing to fix there.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including the new
  regression test.
- Manually reproduced the original failure and confirmed the fix with a
  bare `ffmpeg` invocation against a `*.jpg.scratch`-style output path.

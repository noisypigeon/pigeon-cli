# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-23

### Tighten access-key input/output descriptions

Tightens access-key's input and output descriptions to match the terse, noun-phrase style used by the other modules, and clarifies what `is_bucket_scoped=false` actually does (grants full account access via an empty-string bucket grant, rather than being unclear about what "limit" means). No behavior change — descriptions only, no type/default edits.

[#2](https://github.com/noisypigeon/pigeon-tf/pull/2)

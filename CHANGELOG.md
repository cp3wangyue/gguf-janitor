# Changelog

All notable changes to GGUF Janitor are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/).

## [0.1.1] — 2026-09-12

### Added
- License activation dialog inside the GUI (click the "License: Free" badge),
  matching the CLI `activate` command.

## [0.1.0] — 2026-09-12

### Added
- Automatic discovery of Ollama (blobs + manifest tags), LM Studio
  (`.lmstudio` and legacy `.cache/lm-studio`), HuggingFace cache
  (`HF_HUB_CACHE` honored), custom folders and full-drive loose scans.
- Streaming GGUF v1–v3 header parser: name, architecture, quant, parameter
  count, tensor count, context length; safe limits on hostile headers;
  split/sharded GGUF grouping.
- Duplicate detection: size grouping → streamed XXH3-64 → head+tail 4 KiB
  byte verification.
- Reclaim actions: NTFS hardlink dedupe (re-verified, temp-swap, locked-file
  safe), Recycle Bin deletion, cross-volume verified archive moves with
  JSONL undo manifest. Dry-run everywhere.
- Fit check: weights + KV-cache (GQA-aware) + overhead vs detected RAM /
  nvidia-smi VRAM.
- Windows GUI (egui) with scan progress, duplicate groups, per-model info and
  fit panel; single-file 6.5 MB executable.
- CLI: `scan [--json]`, `dedupe [--dry-run|--yes|--mode hardlink|recycle|
  archive|--dest|--min-size-mb]`, `info`, `fit`, `activate`.
- Offline license gate: free tier dedupes files ≤ 1 GiB; Pro key
  (`GJ-…`, HMAC-verified) unlocks unlimited sizes.
- 30 unit tests covering parser, discovery, dedupe, actions, license, fit.

# Scan index storage

A full scan retains metadata for every readable file and directory. Sampling, directory limits and discarded errors are not used to shrink it. The index retains metadata and bounded project descriptions, not complete documents or transcript bodies. AI summaries still require an explicit action.

- Each full scan builds a private working index. After writes close, supported macOS filesystems compress it losslessly. The system decodes the entire output for a byte-for-byte comparison before atomic publication.
- Published snapshots open read-only. Preferences, cleanup plans, authority and per-file receipts live in `user-state.sqlite3`. Partial folder measurements commit small deltas, reuse child-folder totals and account for cross-folder hard links without expanding the full snapshot.
- The next full scan supersedes its predecessor and old deltas. Cancellation, failure and low space retain published results. Startup only reclaims reserved temporary indexes with a valid application manifest whose owner has exited.
- The UI reports allocated disk bytes, including mutable state and WAL files. A compressed file's logical length can be much larger.
- Missing bulk metadata, unsupported filesystems and unusually large directories fall back to per-file reads. Permission errors and files changing during traversal remain explicit unreadable entries.

Existing indexes upgrade in the background. Conversion needs temporary compressed output space; full scans still need an uncompressed working index plus a disk reserve. Unsupported compression falls back to ordinary storage without changing cleanup preview and confirmation.

Validation: `cargo test --locked --manifest-path native/Cargo.toml`, `node --test native/tests/progress.test.mjs`, `scripts/test-native-smoke.py`, and `scripts/test-native-parity.py`. `scripts/benchmark-native-scan.py` alternates native binaries on identical owned files and checks full paths, sizes, classification and association fingerprints.

The type-4 decmpfs resource-fork layout follows brkirch's [afsctool (GPL-3.0)](https://github.com/jrk/afsctool/blob/d81014e9b6df9582da2a523e89b06523af517496/afsctool.c). The streaming encoder is in `native/src/fs_compression.rs`; no third-party executable is bundled. This repository remains GPL-3.0-only.

Published snapshots use `index-v4.sqlite3`. Upgrades checkpoint the legacy WAL before renaming; separate versioned filenames prevent an older app from replacing retained settings or history with freshly created tables.

Sparse indexes are maintained while walking; only the two dense indexes are bulk-built afterward. This avoids rereading the entire metadata table for each small partial index. Scan timing follows monotonic worker measurements across system sleep or wall-clock changes.

Elapsed time continues during blocked operations without inventing filesystem activity. Completion includes parent-process publication and cache commits.

Read failures are rechecked after a scan. Diagnostics distinguish access denial, missing paths, cloud timeouts and format mismatches; readable directories are rescanned, with sizes and categories updated through small deltas. Original failures and recovery results stay in the local state database. See [read diagnostics](read-recovery.en.md). Packaged recovery is verified by `scripts/test-native-recovery.py`.

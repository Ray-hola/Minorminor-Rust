# Read diagnostics and recovery

Completed scans automatically recheck recorded failures. **Read diagnostics → Check and retry** rechecks them after access grants, synchronization or directory changes. It only processes paths recorded by the current scan and never performs cleanup.

| Category | Evidence and handling |
| --- | --- |
| Access denied | Preserve actual `EACCES` / `EPERM` results. `EPERM` alone cannot identify the specific macOS privacy service or system protection. |
| Missing path | Recheck `ENOENT` / `ENOTDIR` / `ESTALE`; verify the physical Data-volume path and indexed identity before treating an absent root alias as disappearance; retain the original failure and current state. Missing is not reported as a successful read. |
| Cloud timeout | Cloud directory reads run in isolated processes, with a ten-second idle timeout and one retry. Incomplete reads remain visible; placeholder contents are not deliberately downloaded. |
| Format mismatch | Check the SQLite header before parsing. Empty files and single absolute-path references are not SQL databases. Unknown formats and corrupt databases remain diagnostics. |
| Recovered | Fully rescan directories that became readable; refresh a recovered file through its parent directory. Atomically update sizes, categories and error counts without following links or crossing volumes. |

Recovery runs under the App's existing access grants. Filesystem probes have process-level timeout boundaries; the background task can be stopped. Recovery does not elevate privileges, change file permissions or bypass macOS protection. Some system directories remain restricted even with Full Disk Access.

The compressed published index stays read-only. Small transactional deltas and local audit records store the changes. A subsequent full scan replaces the old snapshot and diagnostic history. This mechanism sends no file contents to models: document access stays disabled for AI classification, and AI summaries still require explicit selection and confirmation.

Validation: `native/tests/read_recovery.rs` covers classification, rescanning, counts, restart and repeated checks; `native/tests/overlay.rs` covers rollback, persistence, materialization and preservation of compressed main storage; `scripts/test-native-recovery.py` exercises the packaged API against disposable restricted directories.

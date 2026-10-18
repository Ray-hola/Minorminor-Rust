# Desktop app

The interface follows Tempo’s warm paper (`#F5F1E8`), brown ink and terracotta accent (`#B5532F`). It uses the same system monospace stack as Tempo: SF Mono / Menlo with PingFang SC for Chinese, without online font loading.

The app icon, sidebar mark and loading mark share `public/icon.svg`; the macOS build exports PNG and ICNS automatically to keep their colors consistent.

ICNS resource names include a content hash, and the build refreshes application registration so Finder and Dock can pick up icon updates instead of retaining stale artwork.

[Project home](../README.en.md) · [简体中文](desktop.md)

## Requirements

- macOS 12 or later. Verified on Apple Silicon; Intel builds have not been verified.
- Source development requires Git, stable Rust 1.88+ and Xcode Command Line Tools (`xcode-select --install`). See the [official Rust installation guide](https://rust-lang.org/tools/install/).
- The first build downloads Cargo dependencies. A built `.app` needs neither Node.js nor Rust installed.

```sh
git clone --recurse-submodules https://github.com/Ray-hola/Minorminor-Rust.git
cd Minorminor-Rust
./scripts/run-native.sh
```

`start.command` uses the same entry point. If `vendor/mole/` is empty, run `git submodule update --init --recursive`.

## Workflow

Choose a scan scope. When scanning finishes, expand a project or tool directory to inspect folder and file sizes. Caches and logs are the default review categories; sessions and memory require a separate opt-in. Select individual files in the preview, choose Trash or separately confirm permanent deletion, then check the receipt for original path, target and outcome.

Choose Simplified Chinese or English in the sidebar. Switching reloads the interface and clears unconfirmed selections while background jobs continue. The preference is stored with app records. File names, content, project descriptions, model responses and raw command output retain their original language.

Unknown data, credentials, databases and WAL/SHM sidecars, models and dependencies are retained. Recent, changed, busy or protected files are skipped. Project roots are not moved and Trash is not emptied. Rescan to update sizes after cleanup. Use the receipt to restore files manually from Trash; Finder's Put Back is not guaranteed.

AI analysis sends only metadata you review; document reading and tool invocation remain disabled. AI summaries sample only the resource you select and confirm: up to 12 content files, 160 entries, 32 directories, 3 levels and 48,000 characters. Document extraction and compressed reads have additional bounds; the preview shows actual coverage. Models cannot select or clean files automatically.

## Records and access

Records live in `~/Library/Application Support/com.minorminor.desktop/`, including the SQLite index, settings and file receipts. API keys stay in process memory. The service listens on an ephemeral `127.0.0.1` port. Closing the window during a scan hides it while scanning continues; reopen it from the Dock. Stop or Quit ends the task. Other job types still stop when the window closes. Administrator privileges are not requested automatically.

Diagnostics distinguish permission denials, missing paths and other read failures. Settings → File access opens macOS settings, where you can decide whether to grant Full Disk Access before rescanning.

This repository maintains the Rust + WebKit desktop application. The [Node.js Web edition](https://github.com/Ray-hola/Minorminor) is maintained in a separate repository with its own records, APIs and cleanup workflows.

## Validate and build

```sh
cargo test --locked --manifest-path native/Cargo.toml --no-default-features
cargo check --locked --manifest-path native/Cargo.toml
./scripts/build-macos.sh
python3 scripts/test-native-smoke.py
```

The last command uses Python 3 to test the packaged app with disposable fixtures.

The scanner reuses SQLite statements and credential-redaction patterns, resolves projects by associated path, and aggregates folder sizes and read-error counts together. Each scan still traverses the selected scope in full, with no new file-count limits, depth limits or exclusions and no skipping based on a previous index. Read failures remain in diagnostics. Cleanup identity checks and confirmation are unchanged.

Compare two builds using disposable fixtures:

```sh
python3 scripts/benchmark-native-scan.py --baseline /path/to/previous/Minorminor --runs 3
```

The benchmark checks every file, directory and logical size, then compares storage accounting, hard links, categories, projects, tools and session associations across both indexes. It creates only temporary files and never scans or cleans personal files. It measures traversal and local classification with optional network AI disabled.

Local release benchmark on 2026-10-19: 61,204 files, 2,007 directories and 1,200 session associations, alternating three runs per binary on the same fixture. Median time fell from 18.754 s to 2.158 s (about 8.7×), with identical indexes across all six runs. These are local fixture measurements; actual times depend on storage, cloud directories and permissions, and do not imply the same speedup on every Mac.

The build creates `dist/Minorminor.app` for the current architecture. The build copies the pinned Mole engine, protection script and license files, then verifies an ad-hoc signature. It is not Apple Developer ID signing or notarization. The [v0.2.0 preview](https://github.com/Ray-hola/Minorminor-Rust/releases/tag/v0.2.0) provides an Apple Silicon build; no universal binary or notarized installer is available.

UI translations live in `public/locales/en.js`. Use `tr` / `ui` for authored text; interpolate file paths and content unchanged instead of translating the whole DOM.

## Scan progress

A sticky status area shows the current step, file count, bytes, elapsed time and path. Failures remain visible. Walking uses an indeterminate indicator because the total is unknown; folder measurement shows a real completed/total count. Status and cancellation remain available when the index is busy. See [feature differences](native-parity.en.md).

## Restored workflows

See the [migration and verification checklist](native-parity.en.md). Trash remains the default; permanent deletion and full Mole clean require separate explicit confirmation. The clean page provides four Mole previews and selectable frozen cache / log lists. Full clean uses the complete upstream rule scope, beyond the selectable subset.

AI advice supports tools, file plans and Mole plans in merged batches. Optional automatic candidate identification uses metadata only. Ollama model details and running state are shown before exact API removal. Projects, tools, diagnostics, advice, plans and receipts support JSON / CSV exports. Settings includes paginated Web history import and viewing; old cleanup authorizations are never restored.

Additional verification:

```sh
node --test native/tests/progress.test.mjs
python3 scripts/test-native-parity.py
```

## Estimated scan progress

Before traversal, the app reads total and available disk space. Used space provides the initial byte estimate; a complete previous scan of the same root takes precedence. The progress bar is labeled **Estimated progress**, reserves room for classification, folder measurement and model detection, and reaches 100% only on successful completion. Actual counts and bytes remain visible. The estimate is not an exact time remaining.

Legacy scan records without a size estimate can use the current capacity snapshot to display an estimated percentage. Completed scan results and their percentage remain available after restart or another task. Use `Minorminor-Rust/dist/Minorminor.app` as the desktop application.

Refocusing the window synchronizes background tasks. The latest scan restores its actual scope, including data-volume scans started in the background. Routine progress refreshes preserve a manually chosen scope for the next scan.

After a successful scan, the overview updates scanned bytes and read errors before loading project and tool details, so completed results no longer appear as pending during that load.

Dedicated indexes serve installation candidates, memory, directories and hard links at large scales. Their first migration runs in the visible scan preparation stage, without blocking window initialization. Reverse path order rolls descendants up before ancestors without sorting every long path by length. Installation discovery, memory association and project assignment also report their current path.

Project storage streams indexed entries once per project instead of repeatedly sorting millions of file identities into temporary tables. Hard-link totals, category counts and duplicate identities from files moved during scanning retain their accounting rules.

On scans with millions of files, project assignment and the first detail load can still take minutes. Percentages are estimates; paths and counts can pause during some database operations. Full traversal covers accessible paths on the selected volume, without following symbolic links or crossing mounts. Permission failures and cloud timeouts do not imply zero storage.

## Contributions and synchronization

Ray-hola maintains scope, merges and releases. Run the checks above when changing behavior and update both language guides. Tests use disposable fixtures only. Group commits by purpose and describe the actual change and verification.

Before pushing, inspect the diff, remote and branch. Exclude `.runtime/`, `dist/`, `native/target/`, secrets and personal scan data. Keep the pinned Mole revision and `UPSTREAM.json` aligned. Read back the remote commit and its Actions run after pushing. Formal releases require separate version, change notes and signing checks; Actions artifacts are not notarized installers. Restore shared changes with `git revert <commit>` rather than rewriting published history.

Reports should include macOS version, chip architecture, reproduction steps and error codes, with private paths and content removed. Never attach raw scan databases or API keys to a public issue.

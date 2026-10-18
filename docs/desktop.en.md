# Desktop app

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

Choose a scan scope. When scanning finishes, expand a project or tool directory to inspect folder and file sizes. Caches and logs are the default review categories; sessions and memory require a separate opt-in. Select individual files in the preview, confirm moving them to Trash, then check the receipt for original and destination paths.

Choose Simplified Chinese or English in the sidebar. Switching reloads the interface and clears unconfirmed selections while background jobs continue. The preference is stored with app records. File names, content, project descriptions, model responses and raw command output retain their original language.

Unknown data, credentials, databases and WAL/SHM sidecars, models and dependencies are retained. Recent, changed, busy or protected files are skipped. Project roots are not moved and Trash is not emptied. Rescan to update sizes after cleanup. Use the receipt to restore files manually from Trash; Finder's Put Back is not guaranteed.

AI analysis sends only metadata you review; document reading and tool invocation remain disabled. AI summaries sample only the resource you select and confirm: up to 12 content files, 160 entries, 32 directories, 3 levels and 48,000 characters. Document extraction and compressed reads have additional bounds; the preview shows actual coverage. Models cannot select or clean files automatically.

## Records and access

Records live in `~/Library/Application Support/com.minorminor.desktop/`, including the SQLite index, settings and file receipts. API keys stay in process memory. The service listens on an ephemeral `127.0.0.1` port. Closing the window stops background jobs. Administrator privileges are not requested automatically.

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

The build creates `dist/Minorminor.app` for the current architecture. The build copies the pinned Mole engine, protection script and license files, then verifies an ad-hoc signature. It is not Apple Developer ID signing or notarization. No universal binary or formal installer has been published.

UI translations live in `public/locales/en.js`. Use `tr` / `ui` for authored text; interpolate file paths and content unchanged instead of translating the whole DOM.

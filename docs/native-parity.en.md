# Rust migration and verification

[简体中文](native-parity.md) · [Desktop guide](desktop.en.md)

The local Web implementation is the reference for scanning, cleanup, project analysis and AI workflows. Previously missing workflows have been implemented in Rust. The apps keep separate runtime records; Web JSON history can be imported, but old previews never restore cleanup authority.

| Capability | Rust implementation and verification |
| --- | --- |
| Full synchronized scan | Home or startup data volume; one snapshot for directories, projects, tools, associations and models. Tested beyond 60,000 files, including hidden and deeply nested projects; traversal has no count or depth cutoff. |
| Progress and cancellation | Persistent stage, counts, bytes, elapsed time and path; status and cancellation remain responsive under an index write lock. The previous complete snapshot stays available during rescans. |
| Folder and file usage | Allocated and logical size, categories, search, size / name / date sorting, complete pagination; targeted folder measurements update projects and ancestors. |
| Projects and purposes | Project markers, installation evidence, explicit working directories, nearest real project and encoded memory folders. Independent child projects have separate browsing and cleanup scopes. |
| Known and unfamiliar harnesses | Catalog, environment overrides, XDG, npm / Python / IDE installation metadata, command locations, native application identities and Electron ASAR manifests. SDK dependencies alone do not establish a harness identity. |
| Candidate identification | Bounded structural evidence contains field names, features, table names and necessary local project references. Optional AI batches, fingerprint cache and uncertainty markers; AI cannot create paths, rules or deletion authority. |
| Association recovery | Private consistent main DB + WAL snapshots, relative paths, file URIs and unique compressed session recovery within a tool. Missing paths, permissions, ambiguity, corrupt archives and unsupported databases are explicit. |
| Visual cleanup | Frozen file identities, selection, pagination, expiring one-use confirmation; caches, logs, sessions, memory and downloaded installers. Personal history needs an additional acknowledgement. |
| Cleanup methods | Trash by default; permanent deletion requires a separate acknowledgement. Changed, open, protected, credential, dependency, database, model and backup files remain. Per-file original path, target, status and reason are recorded. |
| Mole previews | clean / purge / installer / optimize with background output. |
| Selected Mole cleanup | Fresh ledger, frozen groups and file list, supported cache / log scopes only; changed groups are retained and unselected scopes remain untouched. |
| Full Mole clean | Separate confirmation of the entire permanent-clean scope; pinned command, one-use preview, raw output and audit records. Other Mole commands retain their original preview-only behavior. |
| AI risk advice | Tools, current file plan or Mole plan; 20-item batches, merged results, keep / review / consider, risk, reasons, uncertainty, filters and exports. |
| AI permissions | Analysis never sends document bodies and rejects extra read or tool requests. File / folder summaries require explicit selection, coverage preview and confirmation; nested projects are excluded from parent summaries. |
| Local models | Ollama details, usage, running state and model selection; identity and running state are rechecked before exact API removal. Chat Completions compatible services are also supported. |
| History, exports and language | JSON / CSV for projects, tools, links, diagnostics, advice, plans and all receipts. Paginated cleanup and imported Web history; Simplified Chinese and English. |
| Interface | Existing visual style, reachable fixed preview / confirmation actions and collapsible AI navigation. |

## Verified on 2026-10-19

Apple Silicon macOS:

- **53 Rust tests passed**: 19 parity, 19 safety, 5 review, 5 progress and 5 unit tests.
- **12 progress UI tests passed**.
- Packaged application smoke test passed, including real worker processes, cancellation under a write lock, pinned Mole protection, Trash receipts, rescans and persisted language.
- **13 packaged parity workflows passed** with local mock AI / Ollama services: AI batches and cache, permission refusal, selected summaries, diagnostics, history import, exports, protected model removal, four Mole previews, selected cleanup, whole cleanup, cancellation receipts and restart invalidation.
- Native macOS / WebKit window checked: scan status, project categories, folder sizes, 130-file paginated previews, remeasurement, summary preview and result, candidate metadata, AI advice, association diagnostics, Mole plans and confirmation controls, and bilingual navigation.
- Release `.app` build and ad-hoc signature verification passed. Sources: [`native/tests/`](../native/tests/), [`test-native-smoke.py`](../scripts/test-native-smoke.py), [`test-native-parity.py`](../scripts/test-native-parity.py). CI runs the same checks.

Cleanup, permanent deletion and model removal used disposable fixtures and local mock services only. These checks do not guarantee every Mac, unknown harness or cloud provider will be identified correctly. Intel, real stalled cloud providers and actual model downloads / removal have not been verified on hardware. macOS access permissions, mounts and filesystem state still apply and are reported in results and diagnostics.

Estimated progress now captures total and available space before every scan, preferring complete same-scope history over volume used space. Post-traversal stages reserve progress; only successful completion reaches 100%. Cancellation or failure never fakes completion.

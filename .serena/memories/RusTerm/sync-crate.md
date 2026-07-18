# rusTerm: Encrypted Config Sync (`rusterm-sync` crate)

## What it does
Synchronizes `settings.json` to encrypted cloud stores (GitHub Gist, iCloud/local folder, generic HTTP).

## Location
- `crates/rusterm-sync/` — new crate added 2026-07-18
- Files: `src/lib.rs`, `src/error.rs`, `src/backend.rs`, `src/config.rs`, `src/manager.rs`, `src/backends/{mod,gist,http,local_folder}.rs`
- Registered in root `Cargo.toml` (workspace member + `rusterm-sync = { path = ... }` workspace dep)

## Security model (hard constraints)
1. **Remotes only store ciphertext.** Entire `settings.json` wrapped in outer AES-256-GCM before upload. Blob format: `b"RSTERM01" (8B) | version (1B) | reserved (3B zero) | nonce ‖ ct ‖ tag`. Remotes see no metadata (hostnames, IDs, tags, expect regexes).
2. **Master key never leaves local machine.** Key is `ConfigManager::master_key()` (derived from master password via Argon2id). Never serialized, never uploaded.
3. **Equivalent security across backends.** All backends receive byte-identical ciphertext via same `rusterm_crypto::encrypt_data`.

## Why two layers
- Inner (existing): field-level AES-256-GCM in `settings.json` — protects secrets if local file exfiltrated.
- Outer (new sync layer): wraps entire JSON envelope — protects metadata from cloud provider.
- Different threat models, compose without interfering.

## Key types
- `EncryptedSyncBackend` trait: `async fn push(&self, data: &[u8]) -> Result<()>` / `async fn pull(&self) -> Result<Option<Vec<u8>>>` (`Option` = `None` if remote empty).
- `BackendKind`: tagged dispatch enum over `Gist(GistBackend) | LocalFolder(LocalFolderBackend) | Http(HttpBackend)`. Implements `EncryptedSyncBackend` itself.
- `SyncConfig { target: Option<SyncTarget>, blob_name: String }` — user-facing, serde-tagged `SyncTarget::{Gist, LocalFolder, Http}`.
- `SyncManager<'a>`: holds `&'a ConfigManager` + `BackendKind`. Entry points: `push()`, `pull()`, `pull_bytes()` (dry-run).

## Cross-crate change
Added `ConfigManager::config_path()` public getter in `crates/rusterm-core/src/config_manager.rs` so `rusterm-sync` doesn't duplicate path resolution logic.

## Backends
- `GistBackend`: REST API (`POST /gists` create, `PATCH /gists/{id}` update, `GET /gists/{id}` read). Bearer auth. Auto-creates gist on first push if `gist_id` is None. Content base64-encoded (gist API requires UTF-8 strings).
- `LocalFolderBackend`: `fs::write` to a local folder (covers iCloud on macOS via `~/Library/Mobile Documents/com~apple~CloudDocs/`). Atomic write via temp+rename. Expands `~`.
- `HttpBackend`: `PUT {url}/{blob_name}` / `GET {url}/{blob_name}`. Optional bearer. 404 → `Ok(None)`.

## Tests (10 passing)
- `local_folder`: round_trip, creates_missing_folder, overwrites_existing_blob
- `gist`: base64_roundtrip
- `http`: blob_url_joins_correctly, blob_url_strips_trailing_slash (full round-trip left as integration test — toy HTTP server was too flaky)
- `manager`: magic_is_stable, sync_config_serde_round_trip, push_pull_round_trip_via_temp_folder, end_to_end_folder_round_trip

## Pitfalls learned
- `#[serde(default = "true")]` is invalid syntax — must use a function path: `#[serde(default = "default_true")]` + `fn default_true() -> bool { true }`.
- Writing a half-baked HTTP test server in-process is brittle — prefer real/mock servers via `wiremock` for integration tests.
- Don't duplicate `ConfigManager::resolve_config_path` logic — add a public getter on `ConfigManager` instead.

## Open follow-ups (not done, intentional scope limits)
- CLI wiring (no `rusterm-app` integration yet — `SyncManager` is library-only)
- Move gist PAT / HTTP bearer token to OS keyring (`rusterm_crypto::KeyringStore`) — currently inline in `SyncConfig`
- Conflict detection / 3-way merge on pull — currently pull overwrites local (caller's responsibility to back up)
- `GitRemoteBackend` for arbitrary git remotes via `git2` — would let gist use HTTPS clone instead of REST API. Not added (would add heavy `git2` dep); `HttpBackend` covers most "other online encrypted library" cases.

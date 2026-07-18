# Encrypted Config Sync

RusTerm can synchronize your `settings.json` to an encrypted cloud-backed
store so the same config (connections, OneKeys, master-password hash) is
available across machines.

## Security model

The sync layer is built around three invariants. Violating any of them is a
bug.

1. **Remotes store only ciphertext.** Before anything leaves your machine,
   the entire `settings.json` is wrapped in a second layer of AES-256-GCM.
   The remote sees an opaque blob:

   ```
   "RSTERM01" (8B magic) | 0x01 (version) | 3B reserved | nonce ‖ ct ‖ tag
   ```

   No hostnames, no connection names, no tags, no expect regexes, no IDs —
   not even the JSON envelope structure is visible to the cloud provider.

2. **The decryption key never leaves the local machine.** The key is your
   master key, derived locally from your master password via Argon2id (see
   `rusterm_crypto::derive_key`). It is held in memory only while sync is
   running and is never serialized, never uploaded, and never placed in any
   backend.

3. **Equivalent security across backends.** Gist, iCloud, and arbitrary HTTP
   endpoints all receive byte-identical ciphertext produced by the same
   primitive, so the choice of backend does not change the threat model.
   Switching from Gist to iCloud does not weaken (or strengthen) security.

### Why two layers of encryption?

`settings.json` already encrypts *sensitive fields* (passwords, key
passphrases, OneKey `send` values) with field-level AES-256-GCM. That layer
protects secrets if the local file is exfiltrated from disk.

The sync layer adds a second, *transport* encryption around the **entire**
file. This protects envelope metadata (which hosts you connect to, your
group/tag taxonomy, expect regexes that may encode business context) from
the cloud provider. The two layers serve different threat models and compose
without interfering.

## Backends

| Backend       | Use case                                                | Auth                |
|---------------|---------------------------------------------------------|---------------------|
| `gist`        | GitHub Gist via REST API                                | GitHub PAT (gist scope) |
| `local_folder`| iCloud Drive / Dropbox / Syncthing (any sync'd folder) | None (filesystem)   |
| `http`        | Self-hosted cloud with a single-file HTTP endpoint      | Optional bearer     |

### iCloud Drive (macOS)

iCloud Drive appears as a regular folder at
`~/Library/Mobile Documents/com~apple~CloudDocs/`. Point the
`local_folder` backend at a subdirectory there and macOS syncs the file
automatically — no API, no git, no auth needed.

## Configuration

Sync is configured via a `SyncConfig` struct, which can be serialized as
JSON or TOML and placed next to `settings.json` (or embedded inside it).

```json
{
  "target": {
    "kind": "local_folder",
    "path": "~/Library/Mobile Documents/com~apple~CloudDocs/rusterm"
  },
  "blob_name": "rusterm-config.enc.bin"
}
```

### Gist example

```json
{
  "target": {
    "kind": "gist",
    "gist_id": null,
    "token": "ghp_your_pat_here",
    "description": "RusTerm Config",
    "secret": true
  },
  "blob_name": "rusterm-config.enc.bin"
}
```

Leave `gist_id` as `null` on the first push — the backend creates a new
secret gist and stores the resulting ID in memory. Persist it back into
the config before the next run so subsequent pushes update the same gist
instead of creating new ones.

### HTTP example

```json
{
  "target": {
    "kind": "http",
    "url": "https://files.example.com/rusterm",
    "token": "optional-bearer"
  },
  "blob_name": "rusterm-config.enc.bin"
}
```

The backend issues `PUT {url}/{blob_name}` and `GET {url}/{blob_name}`.

## Usage from Rust

```rust
use rusterm_core::ConfigManager;
use rusterm_sync::{SyncConfig, SyncManager};

let cm = ConfigManager::with_master_password("your-master-password")?;
let sync_cfg: SyncConfig = serde_json::from_str(&sync_json)?;

let sync = SyncManager::new(&cm, &sync_cfg)?;

// Upload local settings.json → encrypted blob → cloud.
sync.push().await?;

// Download cloud → encrypted blob → decrypt → overwrite local settings.json.
// Back up the local file first if you have unsaved changes!
sync.pull().await?;
```

## Operational notes

- **Pull overwrites local state.** There is no auto-merge. The CLI should
  prompt before overwriting, or back up `settings.json` to
  `settings.json.bak` first. This is a policy decision left to the caller.
- **Token storage.** The Gist PAT and HTTP bearer token are currently
  expected inline in the config. For production, store them in the OS
  keyring via `rusterm_crypto::KeyringStore` and reference them by name —
  the inline form is a stopgap.
- **Backup your master password.** If you lose it, the synced blob is
  unrecoverable. This is by design — there is no escrow, no recovery path,
  and no way to weaken the encryption without weakening every backend.

## Wire format

```
 offset 0       8         9         12
 ┌─────────────┬─────────┬───────────┬────────────────────────────┐
 │ magic (8B)  │ ver(1B) │ resvd(3B) │ nonce ‖ ct ‖ tag (rest)   │
 │ "RSTERM01"  │  = 0x01 │  = 0x000  │  (AES-256-GCM output)       │
 └─────────────┴─────────┴───────────┴────────────────────────────┘
```

The magic + version let us evolve the format without ambiguity. The
reserved bytes are zero today and reserved for future flags.

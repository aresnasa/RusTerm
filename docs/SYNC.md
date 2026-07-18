# Encrypted Config Sync

RusTerm can synchronize your `settings.json` to an encrypted cloud-backed
store so the same config (connections, OneKeys, master-password hash) is
available across machines.

## Security model

The sync layer is built around four invariants. Violating any of them is a
bug.

1. **Remotes store only ciphertext.** Before anything leaves your machine,
   the entire `settings.json` is wrapped in a second AEAD envelope. The
   remote sees an opaque blob:

   ```
   "RSTERM01" (8B magic) | 0x01 (version) | cipher_id (1B) | 2B reserved |
   nonce ‖ ct ‖ tag (rest)
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
   primitive (selected via `CipherSpec`), so the choice of backend does not
   change the threat model. Switching from Gist to iCloud does not weaken
   (or strengthen) security. The cipher choice (AES-256-GCM vs
   ChaCha20-Poly1305) also does not change the threat model — both consume
   the same 32-byte Argon2id-derived master key.

4. **Only the local master password decrypts the cloud config.** Without
   the master password, the cloud blob is indistinguishable from random
   bytes. There is no escrow, no recovery path, no "forgot password" flow,
   and no way to weaken the encryption without weakening every backend.
   Lose the master password and the synced blob is unrecoverable — by
   design.

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

## Token storage (keychain by default)

Backend tokens (GitHub PATs, HTTP bearer tokens) are stored in the **OS
keyring by default**, not inline in `settings.json`. On macOS this is the
Keychain; on Linux it's the secret service; on Windows it's the credential
manager. The sync layer looks the token up at sync time via
`rusterm_crypto::KeyringStore`.

The config file holds only a `(service, account)` *reference* to the
keychain entry — never the token itself. This means:

- A backup of `settings.json` (synced to the cloud or copied off the
  machine) does **not** contain the GitHub PAT.
- Even a user who unlocks the master password cannot exfiltrate the PAT
  without also having OS-keychain access to the same machine.
- Rotating a token does not require touching `settings.json` — just update
  the keychain entry.

### Default keychain entries

| Backend | Service                | Account   |
|---------|------------------------|-----------|
| gist    | `rusterm.sync.gist`    | `default` |
| http    | `rusterm.sync.http`    | `default` |

### Setting a token

Use `SyncManager::store_token` (or the matching CLI subcommand) to write a
token to the keychain:

```rust
use rusterm_sync::{SyncManager, TokenSlot};

// Writes "ghp_xxx" to the keychain under (rusterm.sync.gist, default).
SyncManager::store_token(TokenSlot::Gist, "ghp_xxx")?;
```

This is the recommended way to set up sync — the token never appears in
`settings.json`.

### Inline tokens (discouraged)

For tests or quick local development, the token can be embedded directly in
the config. This is **discouraged** for production because the token lands
in `settings.json` and would be synced to the cloud (still encrypted at
rest by the master password, but a token leak if the master password is
ever compromised):

```json
{
  "target": {
    "kind": "gist",
    "token": "ghp_your_pat_here"
  }
}
```

A plain JSON string in the `token` field is parsed as `TokenSource::Inline`.
Existing configs with inline tokens require **no migration** — they keep
working as before.

### Keychain token reference (recommended)

```json
{
  "target": {
    "kind": "gist",
    "token": {
      "service": "rusterm.sync.gist",
      "account": "default"
    }
  }
}
```

Both `service` and `account` default if omitted (to `rusterm.sync.gist`
and `default` respectively), so `{}` is also a valid keychain reference.
Use distinct accounts (e.g. `"work"`, `"personal"`) to keep multiple tokens
for the same backend in the same keychain.

## Cipher selection

The `cipher` field on `SyncConfig` selects the AEAD used to wrap the blob
on push. The default is `aes_256_gcm` (the historical cipher, so old blobs
decode transparently). `chacha20_poly1305` is available as an alternative.

| Cipher              | `cipher` value          | Wire id | Notes                              |
|---------------------|-------------------------|---------|------------------------------------|
| AES-256-GCM         | `aes_256_gcm` (default) | `0x00`  | Historical default. Use AES-NI.   |
| ChaCha20-Poly1305   | `chacha20_poly1305`     | `0x01`  | Constant-time, no AES-NI needed. |

Both ciphers take the same 32-byte Argon2id-derived master key, so the
cipher choice does not weaken (or strengthen) the key. The cipher is encoded
in byte 9 of the blob header, so a pulled blob is always decrypted with
whatever cipher it was originally pushed with — changing the `cipher` field
only affects future pushes.

```json
{
  "cipher": "chacha20_poly1305",
  "target": { "kind": "local_folder", "path": "/tmp/rusterm" },
  "blob_name": "rusterm-config.enc.bin"
}
```

## Configuration

Sync is configured via a `SyncConfig` struct, which can be serialized as
JSON or TOML and placed next to `settings.json` (or embedded inside it).

```json
{
  "target": {
    "kind": "local_folder",
    "path": "~/Library/Mobile Documents/com~apple~CloudDocs/rusterm"
  },
  "blob_name": "rusterm-config.enc.bin",
  "cipher": "aes_256_gcm"
}
```

### Gist example (keychain token)

```json
{
  "target": {
    "kind": "gist",
    "gist_id": null,
    "token": {
      "service": "rusterm.sync.gist",
      "account": "default"
    },
    "description": "RusTerm Config",
    "secret": true
  },
  "blob_name": "rusterm-config.enc.bin"
}
```

After writing the config, set the PAT in the keychain:

```rust
SyncManager::store_token(TokenSlot::Gist, "ghp_your_pat_here")?;
```

Leave `gist_id` as `null` on the first push — the backend creates a new
secret gist and stores the resulting ID in memory. Persist it back into
the config before the next run so subsequent pushes update the same gist
instead of creating new ones.

### Gist example (inline token, tests only)

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

### HTTP example

```json
{
  "target": {
    "kind": "http",
    "url": "https://files.example.com/rusterm",
    "token": {
      "service": "rusterm.sync.http",
      "account": "default"
    }
  },
  "blob_name": "rusterm-config.enc.bin"
}
```

The backend issues `PUT {url}/{blob_name}` and `GET {url}/{blob_name}`.
The `token` field is optional — omit it for unauthenticated endpoints.

## Usage from Rust

```rust
use rusterm_core::ConfigManager;
use rusterm_sync::{SyncConfig, SyncManager, TokenSlot};

// One-time setup: write the GitHub PAT to the keychain.
SyncManager::store_token(TokenSlot::Gist, "ghp_your_pat_here")?;

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
- **Token storage.** Tokens live in the OS keychain by default. Use
  `SyncManager::store_token(TokenSlot::Gist, ...)` to set them; the config
  only carries a `(service, account)` reference. Inline tokens in the
  config are a discouraged fallback for tests.
- **Backup your master password.** If you lose it, the synced blob is
  unrecoverable. This is by design — there is no escrow, no recovery path,
  and no way to weaken the encryption without weakening every backend.
- **Rotating the cipher.** To migrate from AES-256-GCM to ChaCha20-Poly1305,
  set `cipher: "chacha20_poly1305"` in the config and push. The new blob
  will be ChaCha20-Poly1305; the old AES-256-GCM blob is overwritten. Until
  you push, pulls will keep decrypting the existing blob with whatever
  cipher it was pushed with — there is no need for a migration tool.

## Wire format

```
 offset 0       8         9         10        12
 ┌─────────────┬─────────┬─────────┬─────────┬────────────────────────────┐
 │ magic (8B)  │ ver(1B) │cipher(1B)│ rsvd(2B)│ nonce ‖ ct ‖ tag (rest)  │
 │ "RSTERM01"  │  = 0x01 │ = 0x00  │ = 0x000 │  (AEAD output)             │
 │             │         │ = 0x01  │         │  (0x00=AES-256-GCM,        │
 │             │         │         │         │   0x01=ChaCha20-Poly1305)  │
 └─────────────┴─────────┴─────────┴─────────┴────────────────────────────┘
```

The magic + version let us evolve the format without ambiguity. The byte at
offset 9 is the cipher id:

- `0x00` — AES-256-GCM (the historical default; all v1 blobs predating
  cipher selection had `0x00` here, so they decode transparently).
- `0x01` — ChaCha20-Poly1305.

Unknown cipher ids are rejected with `SyncError::UnsupportedCipher` rather
than silently guessed, so a future cipher addition cannot corrupt an old
client's view of the blob. The 2 bytes at offsets 10–11 remain reserved
(zero) for future use.

The remainder is the standard `nonce ‖ ciphertext ‖ tag` blob produced by
`rusterm_crypto::encrypt_with`. The 12-byte nonce size is shared by all
supported ciphers.

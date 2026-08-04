# ZMODEM (lrzsz rz/sz) + xterm support

## Goal
Add ZMODEM file-transfer support (interoperate with system-installed `lrzsz` `rz`/`sz`) and ensure xterm-compatible terminal type. User: "添加 lrzsz，xterm支持，需要能够使用 lrzsz".

**STATUS: Working** — `sz file` on a remote host now triggers a save dialog and downloads the file correctly.

## Approach
**Pure-Rust ZMODEM protocol implementation** (NOT shelling out to a local `lrzsz` binary). The remote `rz`/`sz` emits protocol frames; RusTerm parses + responds to them in-process. This matches how iTerm2 / WindTerm integrate ZMODEM.

## New crate: `rusterm-zmodem` (`crates/rusterm-zmodem/`)
Pure-Rust, fully unit-testable, no UI dependencies.

### Modules
- `crc.rs` — CRC-16/ACORN (poly 0x1021, init 0) + CRC-32/ISO-HDLC (poly 0xEDB88320). `const fn` table builders. Known vectors: `crc16_init("123456789")==0x31C3`, `crc32_init("123456789")==0xCBF43926`.
- `frame.rs` — `FrameType` enum (18 variants), `CrcMode` (Crc16/Crc32), `HeaderFrame`, `DataEnd` (ZCRCE=0x68/ZCRCG=0x69/ZCRCQ=0x6A/ZCRCW=0x6B — actual wire values), `ZmodemFrame` (Header | Data). ZDLE escape/encode/decode. `encode_hex_header` / `encode_bin_header` / `encode_bin32_header` / `encode_data_block` / `encode_data_subframe` builders. `zdle_decode_n` decodes exactly N bytes.
- `parser.rs` — `Detector` streaming state machine. States: Idle, Pad1, Pad2, PadDle, InFrame, **InData** (data subframe mode). `feed(bytes) -> (passthrough, Vec<Detection>)`. Detects hex/binary/binary32 headers + data subframes, validates CRC, cancels on 8×CAN. Suppresses inter-frame noise when armed. Uses recursive `process_byte()` to re-feed remaining bytes after a frame completes.
- `session.rs` — `ZmodemSession` high-level state machine. `Direction` (Receive/Send), `SessionEvent` (ReceiveOffered/SendOffered/FileOffer/DataReceived{data:Vec<u8>}/Done/Cancelled/Skipped), `Phase` (Init/AwaitFile/AwaitSavePath/Receiving/AwaitRpos/Sending/Done/Cancelled). Tracks `data_offset` from ZDATA headers. Manual `Debug` impl.

### Critical implementation details (FIXED in latest commit)
1. **ZRINIT flags = 0x03** (CANFDX|CANOVIO), NOT 0x21 (which advertised CANFC32, forcing sz to use ZBIN32).
2. **DataEnd wire values = 0x68-0x6B** (ZCRCE='h', ZCRCG='i', ZCRCQ='j', ZCRCW='k'), NOT 0x01-0x04.
3. **Data subframes have NO ZPAD ZDLE leader** — they start directly after a header and end with `ZDLE <frameend> <crc>`. The `InData` state handles this.
4. **`complete_hex` waits for the byte after CR** before returning — otherwise the LF is left in the collector and misinterpreted as the first byte of a data subframe.
5. **`complete_bin` uses `zdle_decode_n`** (decode exactly N bytes) instead of decoding the entire buffer — prevents trailing data subframe bytes from corrupting the header CRC check.
6. **Remaining bytes are re-fed recursively** via `process_byte()` after a frame completes — prevents data subframe loss.
7. **`encode_data_subframe`** (no ZDATA leader, no offset) is used for ZFILE metadata and ZDATA data subframes. `encode_data_block` (with ZDATA leader) is retained but deprecated.
8. **ZDATA header is sent before data subframes** in the send path (`pump_send_blocks` calls `send_zdata` then `encode_data_subframe`).

### Key protocol decisions
- **CRC16 by default**; CRC32 supported but not auto-negotiated (lrzsz always accepts CRC16).
- **ZRPOS deferred**: when ZFILE arrives, session enters `AwaitSavePath` (NOT Receiving). ZRPOS(0) is only sent when the UI calls `set_save_path()` (after the rfd save dialog resolves). This guarantees the file writer is open before `sz` streams data blocks — prevents data loss.
- **FileOffer fires once**: on the ZFILE metadata subframe (which carries "name\0size mtime mode\0"), not on the ZFILE header (which has empty data). The save dialog gets the real filename.
- **Stop-and-wait send**: one ZDATA block per ZACK (not windowed). lrzsz tolerates this.
- **ZDLE decode**: standard `byte ^ 0x40`. High-bit CR variant (0x8D→0x0D) is NOT supported (non-standard; lrzsz uses plain 0x4D for CR).

### 42 unit tests
CRC vectors, ZDLE round-trips, hex/binary header detection (incl. partial frames + CRC rejection), cancel on 8×CAN, receive/send negotiation, file metadata parsing (NUL-separated), session lifecycle, **data subframe detection** (ZFILE+subframe in one chunk, separate chunks, ZDATA+Continue+End, control bytes in payload), **full receive-path integration** (ZRQINIT→ZFILE+metadata→FileOffer, ZRQINIT→ZFILE→ZRPOS→ZDATA+data→ZEOF→Done).

## Integration: `rusterm-ui/src/zmodem.rs`
- `ZmodemSessions` — per-session state holder (`HashMap<session_id, ZmodemSession>` + writers + pending_send). `Debug` derived. Stored on `AppState.zmodem: Arc<Mutex<ZmodemSessions>>` (serde-skip).
- `process_output(state, session_id, data) -> ProcessedOutput` — feeds bytes through the session, returns `passthrough` (for terminal rendering) + `to_pty` (ZMODEM responses) + `events` (UI actions) + `zmodem_active`/`zmodem_finished` flags.
- `dispatch_event(session_id, event, state_handle, input_sender) -> bool` — handles each `SessionEvent`: `SendOffered` → spawn rfd open dialog (reads file, calls `install_send_payload`), `FileOffer` → spawn rfd save dialog (calls `install_receive_path` which opens the writer), `DataReceived` → write payload to the writer, `Done`/`Cancelled` → flush+close writer, remove session, return true.
- `install_receive_path` opens `std::fs::File::create(path)` and stores `Arc<Mutex<Option<File>>>`.
- `install_send_payload` calls `session.begin_send(name, payload)` which emits ZFILE header.

### Wiring in `app.rs`
- `intercept_zmodem(state, input_senders, session_id, data) -> Vec<u8>` helper. Clones the zmodem Arc + input sender, calls `process_output`, injects `to_pty` via sender, dispatches events. Returns passthrough bytes.
- Called in **both** SSH (`start_ssh_connection`) and shell (`start_shell_connection`) `SessionEvent::Output` handlers. **Order: `intercept_zmodem` runs BEFORE `shell_integration_echo_filter`** so ZMODEM binary bytes bypass the echo filter entirely (the echo filter only cares about the startup command echo, which happens once at startup; by the time `sz`/`rz` runs, the echo filter is idle).
- **Disconnect cleanup**: `app.zmodem.lock().remove(&id)` in both SSH and shell `SessionEvent::Disconnected` handlers.

### 7 unit tests in zmodem.rs
Non-zmodem passthrough, ZRQINIT activates + produces ZRINIT, bytes-before-frame passthrough, remove clears state, take_pty_output drains, install_receive_path creates writer + opens file, install_send_payload emits ZFILE header.

## xterm support: `rusterm-proto/src/shell.rs`
- `effective_term_value(env) -> String` pure helper: returns user-supplied `TERM` from `config.env` or `"xterm-256color"` default.
- `ShellConnection::open` now calls `cmd.env("TERM", &effective_term_value(&config.env))` — local shells always get a real TERM (was previously inherited from the GUI app's environment, often unset/dumb). SSH already negotiates via `request_pty(terminal_type, ...)` with `SshConfig.terminal_type` (default "xterm-256color").
- 3 tests (default, user-supplied wins, non-TERM keys ignored).

## Test totals
- rusterm-zmodem: 42 (was 36; +6 data subframe + integration tests)
- rusterm-ui: 673 (was 672; +1 SelectAll test)
- rusterm-proto: 6
- All green. (Pre-existing unrelated failure in `rusterm-relay::command_guard::tests::empty_json_object_is_valid` — not touched.)

## Commits
- `feat(zmodem): add rusterm-zmodem crate with ZMODEM protocol parser` — core crate (36 tests)
- `feat(zmodem): integrate ZMODEM into terminal sessions + xterm TERM` — UI wiring + proto TERM
- `fix(zmodem): fix ZMODEM detection — data subframe parsing, CRC, ZRINIT flags` — the big fix (9 root causes, +6 tests)
- `feat(ui): add Cmd+A select-all-copy and triple-click line select` — session copy + mouse selection improvements

## Known limitations / future work
- **No transfer progress UI panel** yet. Progress is logged (`[ZMODEM] data received offset=… len=…`). The existing `transfers` module (TransferState/TransferJob) could be reused to show a progress bar, but isn't wired up.
- **Serial/Telnet** `SessionEvent::Output` handlers do NOT have the `intercept_zmodem` call (only SSH + shell do). Adding it is a 3-line change per handler if needed.
- **ZSINIT / escctl / crash recovery windows** not implemented (out of scope; lrzsz works without them).
- **CRC32 auto-negotiation** not implemented (CRC16 always used; lrzsz accepts this).
- **Send path block size** is fixed 1024 (lrzsz default); no adaptive sizing.
- **No ZMODEM trigger menu** — transfers start automatically when `rz`/`sz` is run on the remote. No manual "send file" button.

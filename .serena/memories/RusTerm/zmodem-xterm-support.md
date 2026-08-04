# ZMODEM (lrzsz rz/sz) + xterm support

## Goal
Add ZMODEM file-transfer support (interoperate with system-installed `lrzsz` `rz`/`sz`) and ensure xterm-compatible terminal type. User: "添加 lrzsz，xterm支持，需要能够使用 lrzsz".

## Approach
**Pure-Rust ZMODEM protocol implementation** (NOT shelling out to a local `lrzsz` binary). The remote `rz`/`sz` emits protocol frames; RusTerm parses + responds to them in-process. This matches how iTerm2 / WindTerm integrate ZMODEM.

## New crate: `rusterm-zmodem` (`crates/rusterm-zmodem/`)
Pure-Rust, fully unit-testable, no UI dependencies.

### Modules
- `crc.rs` — CRC-16/ACORN (poly 0x1021, init 0) + CRC-32/ISO-HDLC (poly 0xEDB88320). `const fn` table builders. Known vectors: `crc16_init("123456789")==0x31C3`, `crc32_init("123456789")==0xCBF43926`.
- `frame.rs` — `FrameType` enum (18 variants: ZRQInit=0..ZStderr=17), `CrcMode` (Crc16/Crc32), `HeaderFrame`, `DataEnd` (ZCRCE/ZCRCG/ZCRCQ/ZCRCW), `ZmodemFrame` (Header | Data). ZDLE escape/encode/decode. `encode_hex_header` / `encode_bin_header` / `encode_bin32_header` / `encode_data_block` builders.
- `parser.rs` — `Detector` streaming state machine. Scans for frame leader (`ZPAD ZPAD ZDLE <fmt>`). `feed(bytes) -> (passthrough, Vec<Detection>)`. Detects hex/binary/binary32 headers, validates CRC, cancels on 8×CAN. Suppresses inter-frame noise when armed.
- `session.rs` — `ZmodemSession` high-level state machine. `Direction` (Receive/Send), `SessionEvent` (ReceiveOffered/SendOffered/FileOffer/DataReceived{data:Vec<u8>}/Done/Cancelled/Skipped), `Phase` (Init/AwaitFile/AwaitSavePath/Receiving/AwaitRpos/Sending/Done/Cancelled). Manual `Debug` impl (avoids printing send_payload).

### Key protocol decisions
- **CRC16 by default**; CRC32 supported but not auto-negotiated (lrzsz always accepts CRC16).
- **ZRPOS deferred**: when ZFILE arrives, session enters `AwaitSavePath` (NOT Receiving). ZRPOS(0) is only sent when the UI calls `set_save_path()` (after the rfd save dialog resolves). This guarantees the file writer is open before `sz` streams data blocks — prevents data loss.
- **FileOffer fires once**: on the ZFILE metadata subframe (which carries "name\0size mtime mode\0"), not on the ZFILE header (which has empty data). The save dialog gets the real filename.
- **Stop-and-wait send**: one ZDATA block per ZACK (not windowed). lrzsz tolerates this.
- **ZDLE decode**: standard `byte ^ 0x40`. High-bit CR variant (0x8D→0x0D) is NOT supported (non-standard; lrzsz uses plain 0x4D for CR).

### 36 unit tests
CRC vectors, ZDLE round-trips, hex/binary header detection (incl. partial frames + CRC rejection), cancel on 8×CAN, receive/send negotiation, file metadata parsing (NUL-separated), session lifecycle.

## Integration: `rusterm-ui/src/zmodem.rs`
- `ZmodemSessions` — per-session state holder (`HashMap<session_id, ZmodemSession>` + writers + pending_send). `Debug` derived. Stored on `AppState.zmodem: Arc<Mutex<ZmodemSessions>>` (serde-skip).
- `process_output(state, session_id, data) -> ProcessedOutput` — feeds bytes through the session, returns `passthrough` (for terminal rendering) + `to_pty` (ZMODEM responses) + `events` (UI actions) + `zmodem_active`/`zmodem_finished` flags.
- `dispatch_event(session_id, event, state_handle, input_sender) -> bool` — handles each `SessionEvent`: `SendOffered` → spawn rfd open dialog (reads file, calls `install_send_payload`), `FileOffer` → spawn rfd save dialog (calls `install_receive_path` which opens the writer), `DataReceived` → write payload to the writer, `Done`/`Cancelled` → flush+close writer, remove session, return true.
- `install_receive_path` opens `std::fs::File::create(path)` and stores `Arc<Mutex<Option<File>>>`.
- `install_send_payload` calls `session.begin_send(name, payload)` which emits ZFILE header.

### Wiring in `app.rs`
- `intercept_zmodem(state, input_senders, session_id, data) -> Vec<u8>` helper (after `shed_backlog_overflow`). Clones the zmodem Arc + input sender, calls `process_output`, injects `to_pty` via sender, dispatches events. Returns passthrough bytes.
- Called in **both** SSH (`start_ssh_connection`) and shell (`start_shell_connection`) `SessionEvent::Output` handlers, between `shell_integration_echo_filter` and `process_and_render`. Pattern:
  ```rust
  let data = shell_integration_echo_filter.lock().filter(&data);
  if data.is_empty() { continue; }
  let data = intercept_zmodem(state, input_senders, &id, &data);
  if data.is_empty() { continue; }
  state.write().shadow_sandbox.record_output(&id, &data);
  ```
- **Disconnect cleanup**: `app.zmodem.lock().remove(&id)` in both SSH and shell `SessionEvent::Disconnected` handlers.

### 7 unit tests in zmodem.rs
Non-zmodem passthrough, ZRQINIT activates + produces ZRINIT, bytes-before-frame passthrough, remove clears state, take_pty_output drains, install_receive_path creates writer + opens file, install_send_payload emits ZFILE header.

## xterm support: `rusterm-proto/src/shell.rs`
- `effective_term_value(env) -> String` pure helper: returns user-supplied `TERM` from `config.env` or `"xterm-256color"` default.
- `ShellConnection::open` now calls `cmd.env("TERM", &effective_term_value(&config.env))` — local shells always get a real TERM (was previously inherited from the GUI app's environment, often unset/dumb). SSH already negotiates via `request_pty(terminal_type, ...)` with `SshConfig.terminal_type` (default "xterm-256color").
- 3 tests (default, user-supplied wins, non-TERM keys ignored).

## Test totals
- rusterm-zmodem: 36
- rusterm-ui: 672 (was 665; +7 zmodem)
- rusterm-proto: 6 (was 3; +3 TERM tests)
- All green. (Pre-existing unrelated failure in `rusterm-relay::command_guard::tests::empty_json_object_is_valid` — not touched by this work.)

## Commits
- `feat(zmodem): add rusterm-zmodem crate with ZMODEM protocol parser` — core crate (36 tests)
- `feat(zmodem): integrate ZMODEM into terminal sessions + xterm TERM` — UI wiring + proto TERM

## Known limitations / future work
- **No transfer progress UI panel** yet. Progress is logged (`[ZMODEM] data received offset=… len=…`). The existing `transfers` module (TransferState/TransferJob) could be reused to show a progress bar, but isn't wired up.
- **Serial/Telnet** `SessionEvent::Output` handlers do NOT have the `intercept_zmodem` call (only SSH + shell do). Adding it is a 3-line change per handler if needed.
- **ZSINIT / escctl / crash recovery windows** not implemented (out of scope; lrzsz works without them).
- **CRC32 auto-negotiation** not implemented (CRC16 always used; lrzsz accepts this).
- **Send path block size** is fixed 1024 (lrzsz default); no adaptive sizing.
- **No ZMODEM trigger menu** — transfers start automatically when `rz`/`sz` is run on the remote. No manual "send file" button.

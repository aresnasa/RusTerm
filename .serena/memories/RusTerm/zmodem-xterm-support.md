# ZMODEM (lrzsz rz/sz) + xterm support

## Goal
Add ZMODEM file-transfer support (interoperate with system-installed `lrzsz` `rz`/`sz`) and ensure xterm-compatible terminal type. User: "添加 lrzsz，xterm支持，需要能够使用 lrzsz".

**STATUS: Two runtime root causes found via log forensics and FIXED (commits 886cee4 + 1890707). Awaiting user re-test.**

1. **ZHEX/ZBIN32 constants were SWAPPED vs the ZMODEM spec** (`lib.rs`): we had `ZBIN32=b'B'`, `ZHEX=b'C'`; correct is **ZHEX='B' (0x42)**, **ZBIN32='C' (0x43)**. Real lrzsz sends hex headers as `** ZDLE B ...`, which our detector misparsed as a never-completing Bin32 frame. All 44 unit tests were green because encoder+decoder shared the same wrong constants (self-consistent). Root-caused from runtime log line: `rz\r 2a 2a 18 42 30×14 0d 8a 11` + `entering InFrame: kind=Bin32 fmt=0x42`. Regression test `detects_real_lrzsz_sz_wire_bytes` uses the exact captured wire bytes. **Lesson: self-consistent round-trip tests cannot catch wrong wire constants — always test against captured real-world bytes.**

2. **rfd dialogs were spawned with `tokio::spawn`** (worker thread). On macOS, NSSavePanel/NSOpenPanel must be created on the main thread, so the dialog would silently never appear. Changed `spawn_save_dialog` + `spawn_send_file_picker` to `dioxus::prelude::spawn` (main-thread event loop), matching the working rfd usage in `remote_files_panel.rs`. Safe because `dispatch_event` is only called from `intercept_zmodem`, which runs inside the dioxus-spawned SSH/shell event loop tasks; zmodem.rs unit tests never touch these two functions (dioxus spawn panics outside a runtime context).

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
4. **`complete_hex` waits for the byte after CR** before returning — otherwise the LF is left in the collector and misinterpreted as the first byte of a data subframe. Also accepts `0x8A` (high-bit LF variant) in addition to `0x0A` and `0x80`.
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

### 44 unit tests (+2 ZRQINIT with flags + high-bit LF)
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
- rusterm-zmodem: 44 (was 42; +2 ZRQINIT with flags + high-bit LF tests)
- rusterm-ui: 678 (was 673; external process added 5 skin/theme tests)
- rusterm-proto: 6
- All green.

## Commits
- `feat(zmodem): add rusterm-zmodem crate with ZMODEM protocol parser` — core crate (36 tests)
- `feat(zmodem): integrate ZMODEM into terminal sessions + xterm TERM` — UI wiring + proto TERM
- `fix(zmodem): fix ZMODEM detection — data subframe parsing, CRC, ZRINIT flags` — the big fix (9 root causes, +6 tests)
- `feat(ui): add Cmd+A select-all-copy and triple-click line select` — session copy + mouse selection improvements
- `fix(zmodem): add diagnostic tracing + overflow guards + 0x8A LF support` — runtime debugging infrastructure
- `fix(ui): SelectAll now copies full scrollback (not just visible viewport)` — session copy fix
- `fix(ui): selection highlight opacity 0.30 → 0.35 for better visibility` — mouse selection optimization
- `886cee4 fix(zmodem): ZHEX='B' / ZBIN32='C' — constants were swapped vs spec` — THE runtime parse fix (+1 regression test = 45)
- `1890707 fix(zmodem): run rfd dialogs on main thread via dioxus spawn` — macOS dialog thread fix

## Session copy (full scrollback) fix

**Problem**: Cmd+A / Ctrl+Shift+A (SelectAll) only copied the *visible viewport* rows, not the full session scrollback. Users couldn't copy the entire session history.

**Fix**: Added `on_copy_all: EventHandler<()>` prop to `TerminalView`. When SelectAll is triggered, instead of selecting visible rows, it calls `on_copy_all` which:
1. Locks the terminal entry
2. Gets `scrollback_len()` (total scrollback lines)
3. Calls `render_with_scroll(max_scroll)` to render ALL rows (scrollback + visible grid)
4. Extracts text from (0,0) to (last_row, last_col)
5. Copies to clipboard via `copy_text_to_clipboard`

The `copy_text_to_clipboard` and `ClipboardCopyOutcome` were made `pub` in `terminal_view.rs` so `app.rs` can reuse them.

## Mouse selection optimization

- Selection highlight opacity increased from 0.30 to 0.35 (`SELECTION_BG`) for better visibility over both dark and light cell backgrounds.
- Existing mouse selection features (double-click word, triple-click line, drag-select, copy-on-select) are unchanged.

## Diagnostic tracing (added for runtime debugging)

When `sz`/`rz` doesn't trigger a dialog, check the log file (`~/Library/Application Support/rusterm/logs/rusterm.log.*`) for these `[ZMODEM]` / `[ZMODEM-DETECT]` entries:

- `[ZMODEM] no input sender for session ...` — `intercept_zmodem` couldn't find the session's input sender in `input_senders`. This means ZMODEM bytes pass through as text (the #1 suspect).
- `[ZMODEM] ZPAD in data for ...` — ZPAD (0x2A) was found in the output data, indicating a potential ZMODEM frame leader. The hex preview shows the first 40 bytes.
- `[ZMODEM-DETECT] entering InFrame: kind=... fmt=...` — the detector recognized a ZMODEM leader and started collecting a frame.
- `[ZMODEM-DETECT] complete_hex: CRC mismatch! type=... expected=... actual=...` — the frame was received but the CRC didn't match (corruption or wrong CRC algorithm).
- `[ZMODEM-DETECT] complete_hex: detected ...` — a frame was successfully detected.
- `[ZMODEM-DETECT] InFrame overflow (... bytes) — resetting to Idle` — the collector grew beyond 4 KiB without completing a frame (malformed input).
- `[ZMODEM] sending N bytes to PTY for ...` — ZMODEM response (e.g., ZRINIT) was sent back to the remote.
- `[ZMODEM] event for ...: ReceiveOffered` — the session detected a ZRQINIT and is ready to receive.
- `[ZMODEM] event for ...: FileOffer { name: ..., size: ... }` — a file offer was received; the save dialog should spawn next.
- `[ZMODEM] spawning save dialog for ...` — the rfd save dialog was spawned.
- `[ZMODEM] save dialog result for ...: picked/cancelled/failed` — the dialog completed.

### Overflow guards (added to prevent detector stuck states)
- **InFrame**: if the collector exceeds 4 KiB without completing a frame, the detector resets to Idle and flushes collected bytes as passthrough (prevents the detector from silently swallowing all subsequent output).
- **InData**: if the collector exceeds 64 KiB without finding a data subframe terminator, the detector resets to Idle.

## Known limitations / future work
- **No transfer progress UI panel** yet. Progress is logged (`[ZMODEM] data received offset=… len=…`). The existing `transfers` module (TransferState/TransferJob) could be reused to show a progress bar, but isn't wired up.
- **Serial/Telnet** `SessionEvent::Output` handlers do NOT have the `intercept_zmodem` call (only SSH + shell do). Adding it is a 3-line change per handler if needed.
- **ZSINIT / escctl / crash recovery windows** not implemented (out of scope; lrzsz works without them).
- **CRC32 auto-negotiation** not implemented (CRC16 always used; lrzsz accepts this).
- **Send path block size** is fixed 1024 (lrzsz default); no adaptive sizing.
- **No ZMODEM trigger menu** — transfers start automatically when `rz`/`sz` is run on the remote. No manual "send file" button.

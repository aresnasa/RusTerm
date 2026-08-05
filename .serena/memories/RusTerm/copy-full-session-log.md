# RusTerm — 复制完整交互式会话日志 (2026-08-05)

## 目标
用户需要一个**复制完整登录会话**的功能，支持交互式 TTY 逻辑。这超出了已有的 `render_all()`（滚屏+网格的渲染状态复制）——用户要的是**完整 PTY 字节流**（登录提示、交互式问答、命令、输出），包括被滚屏容量截断或被 alt-screen 应用（vim/less/tmux）覆盖的内容。

## 架构决策：基于日志的方法 (A)
选择解密 `.rusl` 会话日志文件，而非在 `Terminal` 里新增"完整会话缓冲区"。原因：
- `.rusl` 文件已经捕获了完整的 IN/OUT PTY 字节流（加密存储）。
- 重复造缓冲区会与日志记录器工作重复，且占用额外内存。
- `decrypt_file` 已存在，只缺"转文本"这一步。

## 改动

### `crates/rusterm-core/src/session_log.rs`
1. **`SessionLog` 新增 `path: PathBuf` 字段 + `path()` 访问器**：之前 `new()` 内部构建文件路径后只把文件句柄交给后台线程，路径丢失了。UI 需要 `path()` 来定位文件做解密。`Debug` impl 也加上 path（不泄露密钥）。

2. **`records_to_transcript(records: &[(String, String, Vec<u8>)]) -> String`**（公开自由函数）：把解密后的记录流转成可读文本。
   - `IN` 记录前缀 `[in] ` 标记（让用户看清自己输入了什么，如密码提示下的输入）；无 `\n` 时补一个。
   - `OUT` 记录原样输出（远端控制自己的换行）。
   - 跳过空记录（如纯清屏 CSI）。

3. **`strip_pty_control(data: &[u8]) -> String`**（公开自由函数）：剥离 ANSI/VT100 转义序列 + 归一化控制字符：
   - CSI (`ESC [...X`)、OSC (`ESC ]...(BEL|ST)`)、DCS (`ESC P...ST`)、单字符 `ESC X` 全部剥离。
   - `\b`（0x08）破坏性退格（不吞换行）。
   - `\r`（0x0d）丢弃（Unix PTY 的 `\r\n` 冗余）。
   - 其他 C0 控制字符（<0x20，除 `\n`/`\t`）丢弃。
   - 非法 UTF-8 → `U+FFFD`（`from_utf8_lossy`）。

4. **`lib.rs` 导出**：`pub use session_log::{SessionLog, records_to_transcript, strip_pty_control};`

### `crates/rusterm-ui/src/components/terminal_view.rs`
1. **`TerminalOverlayKeyAction::CopySessionLog` 新枚举变体**。
2. **快捷键**：`Cmd+Shift+L`（macOS）/ `Ctrl+Shift+L`（其他平台）触发。注意必须带 Shift（`Cmd+L`/`Ctrl+L` 是终端清屏控制序列，不能抢）。
3. **`on_copy_session_log: EventHandler<()>` 新 prop**，在 `TerminalView` 的 keydown 里 `on_copy_session_log.call(())`。
4. **测试**：`copy_session_log_shortcut_detected_on_cmd_shift_l_and_ctrl_shift_l` 验证正/负例（Cmd+Shift+L、Ctrl+Shift+L 触发；Cmd+L、Ctrl+L、Alt+Shift+L 不触发）。

### `crates/rusterm-ui/src/app.rs`（`render_terminal_pane`）
1. 新增 `sid_for_copy_session_log` 克隆。
2. **`on_copy_session_log` 处理器**：
   - 读 `session_logs[sid]`（`Arc<Mutex<SessionLog>>`）+ `config_manager`。
   - 二者任一缺失（app 未解锁 / 该 tab 未建日志）→ 回退到 `render_all()` 复制渲染状态，绝不静默 no-op。
   - `cm.derive_session_key(&sid)` 重新派生每会话密钥（master key + session id 的 Argon2id，确定性可重生）。
   - `log.lock().path()` 取文件路径（不碰后台线程）。
   - `SessionLog::decrypt_file(&path, &key)` → `records_to_transcript(&records)` → `copy_text_to_clipboard(transcript)`。
   - 全程 `tracing::info!("[COPY] CopySessionLog ...")` 记录字符数 / 回退原因 / 失败原因。

## 测试（18 新，全绿）
- `session_log` 模块：23 个测试（原 5 + 新 18）。
  - `strip_pty_control_*`：CSI/OSC(BEL)/OSC(ST)/DCS/单字符 ESC 5 种序列剥离；退格破坏性 + 不吞换行；`\r` 丢弃；其他 C0 丢弃；`\t`/`\n` 保留；UTF-8 保留；非法 UTF-8 → U+FFFD。
  - `records_to_transcript_*`：IN/OUT 顺序输出；ANSI 从 OUT 剥离；空记录跳过。
  - `full_session_log_to_transcript_round_trip`：完整管线——log_output/log_input → close → decrypt_file → records_to_transcript，验证交互式登录流（login→input→Password→input→$→ls→output）保留、SGR/bracketed-paste 序列剥离。
  - `session_log_path_accessor_returns_actual_file`：`path()` 指向真实 `.rusl` 文件、文件名含 session_id。
- `terminal_view` 模块：1 个新测试（`copy_session_log_shortcut_*`）。
- 总计：core 225 passed（原 207 + 18 新），ui 769 passed（原 768 + 1 新）。

## 关键文件
- `crates/rusterm-core/src/session_log.rs` — `path()`/`records_to_transcript()`/`strip_pty_control()`
- `crates/rusterm-core/src/lib.rs:24` — 导出
- `crates/rusterm-ui/src/components/terminal_view.rs` — `CopySessionLog` 枚举 + 快捷键 + prop
- `crates/rusterm-ui/src/app.rs` — `on_copy_session_log` 处理器（约 `render_terminal_pane` 内 `on_copy_all` 之后）

## 安全注意
- `records_to_transcript` **不屏蔽密码**。原始 `IN` 字节就是用户输入的，密码提示下的密码会出现在转录里。这符合"复制我做了什么"的预期，但调用方绝不能把返回文本写日志或发到设备外。
- `.rusl` 文件用每会话 AEAD 密钥加密（master key + session id 的 Argon2id 派生），未解锁无法解密。
- `path()` 只暴露路径字符串，不泄露密钥；`Debug` impl 仍 redact 密钥。

## 关于"chat 保存配置"
用户提到"chat保存的逻辑需要调整""还是没法保存配置"。调查结论：
- **聊天 agent 配置实际能保存**：`settings.json` 的 `chat` 段已正确持久化用户的 `tf-prod` agent（id/name/model/base_url/api_key_id/system_prompt 全在）。
- **真正未保存的是 API key 明文**：`render_agent_config` 的保存按钮（`chat_panel.rs:729-746`）更新 name/model/base_url/system_prompt，但 `draft_api_key` 仅用于反馈消息，从未持久化。代码注释明确 `// API key held in memory only (TODO: keychain)`。
- `AgentConfig::api_key_id` 是 secret store 的外键，明文密钥按设计不入 `settings.json`。要真正"保存 API key"需要实现 keychain 集成（TODO 已标记），这是独立需求，本次未做。

## 转录质量修复（第二轮，2026-08-05）

用户反馈 jumpserver 交互式菜单复制出来"不正确"。日志接线完好（输入 `app.rs:1913`/`2024`，输出 SSH `11858`/Shell `12930`/Serial `13443`/Telnet `13689`），根因有两处，均已修复：

### Fix 1: `strip_pty_control` 光标定位 CSI 产生换行边界
koko/jumpserver 菜单用 `ESC[行;列H` 定位逐行重绘、行间无 `\r\n`，剥掉 CSI 后所有菜单行糊成一串。现在 CSI final byte 为 `H/f/A/B/E/F/J`（CUP/HVP/CUU/CUD/CNL/CPL/ED）时输出 `\n` 边界：
- 用 `out.last().is_some_and(|&c| c != b'\n')` 折叠连续边界（`ESC[2J`+`ESC[H` 只出一个 `\n`），且 buffer 为空时不产生前导空行。
- `C/D/G/K/h/l/m` 等水平/行内/外观序列不产生边界。
- 跳过的旧测试 `strip_pty_control_strips_csi_cursor_moves`（`\x1b[1A` 紧跟 `\n` 后）因折叠守卫结果不变，仍通过。

### Fix 2: 重连不再覆盖 SessionLog
`reconnect_session`（`app.rs:16156`）会再调 `create_terminal_with_size`，原来每次都用新时间戳 `.rusl` 替换 `session_logs[id]` —— 登录流程留在孤立旧文件里，复制只能拿到重连后的内容。现在 `create_terminal_with_size` 开头 `contains_key(&id)` 则直接 return（log 块在函数末尾，return 安全）。输出泵每 chunk 从 map 现查（`logs.get(&id)`），保留旧 entry 意味着重连后输出继续追加到**原文件**，完整历史不断。
- `session_logs` 没有任何 `remove` 路径，tab 存活期内 entry 生命周期安全。

### 新测试（4 个，session_log 模块共 27 个）
- `strip_pty_control_positioned_rows_each_on_own_line`：CUP 定位行各自成行。
- `strip_pty_control_clear_and_home_collapse_to_one_boundary`：`2J`+`H` 折叠、开头无前导空行。
- `strip_pty_control_color_and_inline_sequences_add_no_boundaries`：SGR/水平移动/EL 无边界。
- `records_to_transcript_interactive_tui_login_flow_is_readable`：合成 jumpserver 全流程（banner→login→`[in] ops`→koko 中文菜单帧→`[in] 33`→错误重绘帧），断言顺序+每行独立。

测试：core 229 通过、ui 769 通过。`cargo build -p rusterm-app` clean，二进制已重建。

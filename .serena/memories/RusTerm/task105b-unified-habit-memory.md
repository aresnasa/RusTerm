# 统一习惯记忆层：Telnet + Serial 接入 (Task #105b, 2026-08-04)

## 目标
将 DuckDB 向量习惯记忆层（Task #105 已完成）扩展到所有会话类型：SSH / 本地终端(PTY) / Telnet / Serial。四种后端共享同一条 命令记录 → 习惯记忆 路径。

## 发现
- **协议层已就绪**：`rusterm-proto` 中的 `telnet.rs`、`serial.rs`、`shell.rs` 都是完整实现（不是 stub）。
- **UI 层已就绪**：`app.rs` 中已有 `start_ssh_connection`、`start_shell_connection`、`start_telnet_connection`、`start_serial_connection` 四个函数。
- **SSH 和 Shell 已记录命令**：两者都通过 OSC 133 shell integration 检测命令边界，调用 `record_command` 记录到 DuckDB。
- **Telnet 和 Serial 缺失命令记录**：它们的 `SessionEvent::Output` 处理器只渲染输出，没有 `take_exit_code()`、没有 `pending_exit_check` 队列消费、没有 `record_command` 调用。
- **`pending_exit_check` 队列是会话无关的**：`dispatch_approved_command` → `enqueue_pending_exit` 在所有会话类型的 Enter 键处理中都会调用，所以 Telnet/Serial 的命令已经入队——只是没有人消费。

## 实现

### 1. 共享辅助函数 `process_session_exit_code` (app.rs ~L10995)
从 SSH/Shell 的内联提交逻辑中提取的共享函数：
- 参数：`Signal<AppState>`, `session_id: &str`, `exit_code: Option<i32>`, `log_tag: &str`
- 逻辑：
  1. 从 `pending_exit_check` 队列弹出待处理命令
  2. rc==0 或含 shell operator (|, ||, &&, ;) → 提交到 history DB + DuckDB analytics + 命令纠错学习
  3. rc!=0 → 标记为失败（`mark_command_failed`），从 `command_history` 移除，加入 `recent_failed_commands`
  4. 更新 tab 的 `last_command_status` 徽章
- `log_tag` 用于日志诊断（"SSH"/"LOCAL"/"TELNET"/"SERIAL"）
- `exit_code == None` 时是 no-op（shell 未报告退出码，如不支持 OSC 133 的串口设备）
- 编译注意：`log_tag: &str` 在 spawn 闭包中使用前必须 `.to_string()`（否则 `'static` 约束失败）；`state` 参数必须声明为 `mut state`

### 2. Telnet 命令记录 (app.rs `start_telnet_connection`)
- **注入 OSC 133 shell integration**：复用 SSH 的 `inject_shell_integration_when_quiet` 函数 + `ShellIntegrationEchoFilter`。大多数 telnet 目标（路由器/交换机/BMC 上的 bash/busybox）支持 precmd hook。不支持的设备会忽略脚本，`process_session_exit_code` 是 no-op。
- **输出处理器增强**：
  - 调用 `initial_output_activity_tx.send(())` 通知注入器输出活动
  - 使用 echo filter 隐藏注入脚本的回显
  - 调用 `shadow_sandbox.record_output` + `finish_execution`
  - 提取 `take_exit_code()` + `cwd()`
  - 调用 `process_session_exit_code(state, &id, exit_code, "TELNET")`
  - 镜像 OSC 7 cwd 到 tab
  - 调用 `drive_login_script`（telnet 登录脚本支持）
- **断开处理器**：调用 `shadow_sandbox.fail_execution`

### 3. Serial 命令记录 (app.rs `start_serial_connection`)
- **不注入 OSC 133**：串口目标差异太大（bootloader、MCU REPL、传感器等），注入 shell 脚本可能干扰设备通信。如果用户手动配置了串口 shell 发送 OSC 133 标记，退出码仍会被处理。
- **输出处理器增强**（同 telnet 但无 echo filter / 无 shell integration 注入）：
  - 调用 `shadow_sandbox.record_output` + `finish_execution`
  - 提取 `take_exit_code()` + `cwd()`
  - 调用 `process_session_exit_code(state, &id, exit_code, "SERIAL")`
  - 镜像 cwd 到 tab
- **断开处理器**：调用 `shadow_sandbox.fail_execution`

### 4. 测试
- `state.rs::pending_exit_check_works_for_all_session_backends` — 验证 `pending_exit_check` 队列对所有四种 `SessionType` 都工作（入队 + 出队 + 清空）。这是共享记录路径的基础不变式。
- SSH 和 Shell 的内联提交逻辑保持不变（降低回归风险；telnet/serial 使用提取的辅助函数）。

## 编译修复（非本任务引入的预存问题）
- `PersistedSession` 缺 `replay_ops` 字段 — `session_state.rs` 加了字段但 `app.rs` 测试初始化器没更新。加 `replay_ops: Vec::new()`。
- `conn` 移动后借用 — `replay_ops` 功能在 `conn` 被 `open_connection` 消费后仍访问 `conn.login_script`。在移动前克隆 `conn_for_replay`。

## 验证
- `cargo check --workspace` ✅
- `cargo check --workspace --features rusterm-ui/analytics` ✅
- `cargo test --workspace --lib` — 全部通过 (688 UI + 65 analytics + 185 core + 6 proto + 其余)
- 新测试 `pending_exit_check_works_for_all_session_backends` ✅

## 未做（后续工作）
- 将 SSH/Shell 的内联提交逻辑也重构为调用 `process_session_exit_code`（目前只 telnet/serial 用辅助函数；SSH/Shell 保持内联以降低回归风险）。
- 实时建议管道接入 `suggest_by_context`（Task #105 未完成项）。
- 串口设备的命令边界检测（目前依赖用户手动配置 OSC 133）。

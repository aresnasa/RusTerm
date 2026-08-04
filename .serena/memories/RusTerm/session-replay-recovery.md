# RusTerm — 交互式会话操作录制 + 恢复回放 (2026-08-04)

## 目标
让会话恢复（断线重连 `reconnect_session` + 启动恢复 `restore_sessions`）能"真的恢复" jumpserver 这类交互式 SSH 堡垒机会话：记录用户建立会话状态的交互输入（菜单导航：主机名/编号 + Enter），恢复时按序回放，让用户落回同一目标主机，而不是停在堡垒机菜单。

提交：`d1b4648 "feat: record and replay interactive session ops (jumpserver) on recovery"`。

## 核心设计决策

### 录制什么（establishment 前缀语义）
- 每会话一个 `SessionReplayRecorder { ops: Vec<String>, shell_integrated: bool }`，存于 `AppState.session_replays: HashMap<String, SessionReplayRecorder>`（serde-skip）。
- 仅记录 **Ssh / Telnet** 会话（本地 shell 靠 cwd 恢复；串口无登录流程）。
- 仅记录回车提交的非空行（on_input Enter 分支，`sent > 0` 后）。
- **前缀上限 `REPLAY_MAX_OPS = 10`**：窗口满后永不再录。故稳态 shell 命令不会进入回放日志——重连永远不会回放无界的任意（可能破坏性）命令尾巴。这是安全属性，不是容量优化。
- **凭据过滤**：`credential_kind(current_line).is_some()`（密码/token/用户名提示）时不录制——秘密永不进回放日志（OneKey/登录脚本负责凭据流）。

### 自动区分"堡垒机菜单" vs "真实 shell"（关键启发式）
- **OSC 133;D 退出码 = shell 集成证据**：交互式堡垒机菜单永远不会发出 OSC 133。一旦观察到退出码 → `note_shell_integration_evidence` → 清空已录 ops + 永久停录（`shell_integrated = true`）。集成 shell 的恢复由已有的 cwd 恢复（OSC 7 + `schedule_cd_after_restore`）覆盖。
- 挂钩点：`start_ssh_connection` 的 Output 处理内联 `if exit_code.is_some()` 块 + 外部进程新提的共享 `process_session_exit_code()`（telnet/serial 也走它，已加钩）。
- 已知劣化（可接受）：远端 fish/nu 无集成 → 可能录到最多 10 条普通 shell 命令并回放（有 safety 过滤兜底）；用户自己的远端 dotfiles 发 OSC 133 → 录制被清空，功能退化为不回放（与之前行为相同）。

### 回放引擎（app.rs，`reconnect_session` 之前）
- 常量：`REPLAY_CONNECT_WAIT_SECS=60`（略大于 RECONNECT_WATCHDOG_SECS=45，让 watchdog 的 Disconnected 翻转先触发以干净中止回放）、`REPLAY_POLL_MS=200`、`REPLAY_QUIESCENT_POLLS=4`（800ms 静默 = schedule_cd_after_restore 同等预算）、`REPLAY_OP_TIMEOUT_SECS=20`。
- `filter_replayable_ops(ops, &CommandSafetyChecker) -> (safe, skipped)`：只回放 `SafetyVerdict::Safe`；Warn/Block 直接跳过（无人值守恢复流绝不弹确认框重跑危险命令）。纯函数，有测试。
- `should_schedule_replay(login_script, ops)`：**配置了登录脚本的连接跳过回放**（登录脚本拥有 expect/send 建立流程，回放会双重驱动远端菜单）。空白脚本不算。纯函数，有测试。
- `wait_for_output_quiescence(state, session_id)`：轮询 `SessionTab.version`，连续 4 次 200ms 无变化 = 远端安静（banner/菜单打完）；20s 超时返回 false。**无需改 4 个事件循环**——version 已在每次输出渲染时自增。
- `schedule_replay_after_reconnect(state, input_senders, tab_id, ops)`：spawn 任务：
  1. 等 Connected（`None`＝启动恢复路径连接任务尚未注册状态 → 继续等；`Disconnected`＝重连失败 → 中止）；
  2. safety 过滤；
  3. 终端提示（i18n `session.replaying_ops` / `session.replay_skipped_unsafe`，复用 reconnect 提示的 process_and_render 模式）；
  4. 逐条：等静默 → 校验仍 Connected → 经 **raw input sender** 发 `{op}\r`（不走 request_command_submission —— 不会被再次录制、不进 pending_exit_check 退出码队列：是旧输入回放，不是新命令）。任何一步失败即中止并 warn（`[REPLAY]` 前缀日志）。

### 生命周期
- 录制器**跨断线保留**（disconnect_session_state 不清它——这正是重连回放的数据）；`close_session`/`close_workspace` 删除。
- `reconnect_session`：begin_reconnect 成功后、match 分发前调度回放。
- `restore_sessions` SSH/Telnet 分支：为新 tab_id 重新播种录制器（`session_replays.insert`）+ 调度回放；**回放与 cd 恢复互斥**（`else if let Some(cwd)`）——构造上天然互斥（集成证据会清 ops → 快照 replay_ops 为空），回放防御性优先。

### 持久化
- `PersistedSession` 新增 `#[serde(default)] pub replay_ops: Vec<String>`（rusterm-core/session_state.rs）——随 AES-GCM 加密快照 `session_state.enc` 落盘，legacy 快照反序列化为空。
- `build_session_state` 写入 `replayable_ops(self, &tab.id)`（集成 shell 为空）。
- `plaintext_never_appears_on_disk` 测试扩展了 replay_ops 字符串断言。

## 测试（9 新，全绿）
- `state.rs::session_replay_tests`（6）：按序录制、前缀窗口封顶（rm -rf 不进）、集成证据清空+停录、kind 门控（shell/serial/unknown 拒、telnet 允）、断线保留/关闭删除、build_session_state 含/不含 replay_ops。
- `app.rs::session_replay_engine_tests`（3）：safety 过滤保序、危险 op 跳过、登录脚本/空 ops 跳过调度。
- 全量：rusterm-ui 688 passed、rusterm-core 185 passed（session_state 11 个含扩展）。rustfmt 干净。

## i18n
`session.replaying_ops`（"正在回放 {count} 条已记录的操作以恢复会话状态…"）、`session.replay_skipped_unsafe`（"回放时已跳过 {count} 条潜在危险操作"）。

## 并发工作树注意
外部进程在本次任务期间提交了 `process_session_exit_code()` 重构（telnet/serial 共享退出码处理）并自行修好了 restore_sessions 的 `conn_for_replay` clone。教训依旧：**测试全绿后立即只提交本任务文件**（`git commit -- <files>`）。

## 未来工作
- 回放不覆盖：嵌套 ssh（外层有集成时录制已停）、凭据步骤（需 OneKey/登录脚本配合）、bare-Enter 分页确认（刻意不录以避噪声）。
- 可考虑连接对话框加"恢复时回放交互操作"开关（当前全自动 + 启发式）。
- 静默检测可升级为 OSC 133;A prompt-start 标记（需在会话任务中穿线）。

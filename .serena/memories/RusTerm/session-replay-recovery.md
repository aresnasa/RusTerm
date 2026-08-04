# RusTerm — 交互式会话操作录制 + 恢复回放 (2026-08-04)

## 目标
让会话恢复（断线重连 `reconnect_session` + 启动恢复 `restore_sessions`）能"真的恢复" jumpserver 这类交互式 SSH 堡垒机会话：记录用户建立会话状态的交互输入（菜单导航：主机名/编号 + Enter），恢复时按序回放，让用户落回同一目标主机，而不是停在堡垒机菜单。

提交：`d1b4648`（阶段 1 初版）→ `ded0d49 "fix: make jumpserver interactive-session recovery actually work"`（阶段 4 修复，语义有重大变更，以本文为准）。

## 阶段 4 修复的三个根因（全部有日志实证）
1. **SSH 事件循环从未调用 `drive_login_script`**——只有 shell/telnet 循环挂钩。SSH 连接（堡垒机主通道）配置的登录导航脚本从未运行（全天日志零条 `[LOGIN-SCRIPT]`）。已在 SSH Output 分支 `check_onekey_match` 之后补上。
2. **集成证据清空录制**（原设计缺陷）：用户手动穿过堡垒机落到目标主机后 OSC 133;D 到达 → 原 `note_shell_integration_evidence` 清空菜单导航 ops → 快照 `replay_ops` 恒空。现改为**冻结（freeze）**：保留 pre-evidence establishment 前缀供回放，仅永久停录。`replayable_ops` 不再按 `shell_integrated` 过滤。直连集成 shell 的 evidence 在用户输入前就到 → 冻结前缀为空 → 行为不变（cd 恢复）。
3. **`schedule_cd_after_restore` 盲睡 800ms**：SSH sender 注册需数秒，cd 直接丢（日志 `[RESTORE] no input sender`）。现改为：等 sender + Connected（复用 REPLAY_CONNECT_WAIT_SECS=60/REPLAY_POLL_MS）→ 等输出静默（`wait_for_output_quiescence`，超时非致命照发）→ 发 cd。

## 登录脚本生命周期（阶段 4 新语义）
- `drive_login_script` 的 lazy-init 从 `done_or_absent`（完成即 re-arm，会在用户退回菜单时抢键盘重跑、失败脚本每 30s 重试风暴）改为 **absent-only：每个连接生命周期最多初始化一次**。
- `reconnect_session` 清理块新增 `s.login_scripts.remove(&tab_id)` → 重连的新连接重跑脚本。restore 的新 tab_id 天然 fresh。

## restore_sessions SSH/Telnet 分支的建立优先级（阶段 4）
1. 配置了 login_script → 脚本拥有全部建立流程（在新连接输出循环中自动重跑）；**回放与盲 cd 均跳过**（盲 cd 会打进堡垒机菜单成垃圾输入）。
2. 有 replay_ops → `schedule_replay_after_reconnect(..., ps.cwd)`；若快照还带 cwd（回放落点是集成 shell），回放完成后等静默**追发 follow-up cd**。
3. 纯集成 shell → `schedule_cd_after_restore`。
`reconnect_session` 同样给回放传当前 tab.cwd 作 follow-up。

## 核心设计决策（沿用）
- `SessionReplayRecorder { ops, shell_integrated }`，`AppState.session_replays`（serde-skip）；仅 Ssh/Telnet；仅 Enter 提交非空行（on_input，`sent > 0` 后）；`REPLAY_MAX_OPS=10` 前缀窗口；凭据提示行不录。
- 挂钩点：`start_ssh_connection` Output 内联 `if exit_code.is_some()` + 共享 `process_session_exit_code()`。
- 回放引擎：等 Connected（60s）→ `filter_replayable_ops`（只回 Safe）→ i18n 终端提示 → 逐条等静默（4×200ms，20s 超时）→ raw input sender 发 `{op}\r` → （新增）follow_up_cwd 的 cd。
- `should_schedule_replay(login_script, ops)`：login_script 非空跳过。
- 持久化：`PersistedSession.replay_ops`（serde default）；`build_session_state` 写 `replayable_ops`（现在含冻结前缀）。
- 生命周期：断线保留；close 删除；restore 重播种（`shell_integrated: false`，落到目标后 evidence 再次冻结）。

## 新增工具函数
- `build_restore_cd_command(cwd) -> String`（app.rs）：单引号 + `'\''` 转义，schedule_cd_after_restore 与回放 follow-up 共用，有测试。

## 测试（rusterm-ui 691 passed，rusterm-core 186）
- state.rs `session_replay_tests`：`shell_integration_evidence_freezes_ops_and_disables_recording`（改）、`build_session_state_keeps_frozen_replay_ops_after_integration_evidence`（改）、其余沿用。
- app.rs `session_replay_engine_tests`：+`restore_cd_command_quotes_paths`。

## 用户环境实证（诊断用）
- jumpserver 连接 `jump.zs.shaipower.online` **配置了 login_script**（齐治堡垒机：expect 资产分类列表 → send /cao → 11 → 2），所以该连接走"脚本重跑"路径，不走回放。无脚本的堡垒机连接才走 ops 回放。
- 日志 JSON lines 在 `~/Library/Application Support/rusterm/logs/`；jumpserver 拒绝 exec channel（远端历史拉取失败是已知噪声）。
- "正常退出不弹恢复框" 已由阶段 3（`1bb56de`，2s 变更驱动保存）修复，日志 07:07:21 `Prompting to restore` 实证。

## 未来工作
- "最后一次操作"语义仍是**首次** establishment 前缀：用户在目标主机间切换后的新导航不会更新冻结的 ops。
- 嵌套 ssh、凭据步骤、bare-Enter 分页确认仍不覆盖。
- 静默检测可升级为 OSC 133;A prompt-start 标记。
- bincode 非自描述：改 PersistedSession schema 会破坏旧快照（遗留问题）。

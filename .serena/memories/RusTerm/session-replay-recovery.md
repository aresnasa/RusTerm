# RusTerm — 交互式会话操作录制 + 恢复回放 (2026-08-04)

## 目标
让会话恢复（断线重连 `reconnect_session` + 启动恢复 `restore_sessions`）能"真的恢复" jumpserver 这类交互式 SSH 堡垒机会话：记录用户建立会话状态的交互输入（菜单导航：`/q`、编号 + Enter），恢复时按序回放，让用户落回**最后所在**的目标主机。

提交链：`d1b4648`（阶段 1 初版）→ `51e8089`（阶段 2 启动恢复弹框）→ `1bb56de`（阶段 3 Cmd+Q 弹框：2s 变更驱动保存）→ `ded0d49`（阶段 4 三根因修复）→ **`61a55f3`（阶段 5 "最后一次选择"语义，本文以此为准）**。

## 阶段 5（`61a55f3`）：回放用户的最后一次菜单选择
1. **提示符分类录制**（app.rs on_input Enter 分支，`sent > 0` && 非凭据后）：
   - `prompt_looks_like_shell(current_line)`（state.rs 纯函数）：`➜`/`❯` 开头、`PS ...>`、含 `"$ "`/`"# "`/`"% "`、或行尾 `$`/`#` → shell 类；**裸 `"> "` 故意算菜单**（JMS `Opt>` 优先；fish `~>` 误判为已接受劣化）。
   - shell 类 → `note_shell_prompt_evidence`（冻结，对无 OSC 133 集成的堡垒机目标机也生效）；菜单类 → `record_replay_op`。
2. **菜单重入解冻重录**：`record_replay_op` 遇 `shell_integrated == true` → `ops.clear(); shell_integrated = false;` 再录。快照永远持有最后一次导航序列。"post-evidence shell 命令不进日志"的保证移到调用方（分类器路由）。
3. **录制 ops 优先于纯导航脚本**：`should_schedule_replay(login_script, ops)`——ops 非空时，仅当脚本含 `send_onekey`（`script_handles_credentials`，解析失败算 false）才让脚本赢；纯导航脚本被最后录制覆盖。
4. **脚本压制**：`suppress_login_script(state, sid)` 预插 `LoginScriptRuntime { done: true, steps: [] }`——absent-only lazy-init 使脚本永不启动，避免与回放双重驱动菜单。`restore_sessions` 与 `reconnect_session` 在"回放赢且脚本非空"时调用。
5. **strip_prompt 全角冒号**：end_markers 后加 `line.rfind('：')` 处理（`请选择目标资产：3` → `3`）；**不加 ASCII `": "`**。

## 阶段 4（`ded0d49`）修的三根因（沿用）
1. SSH 事件循环补上 `drive_login_script`（原来只有 shell/telnet 有）；lazy-init 改 absent-only（每连接生命周期一次）；`reconnect_session` 清理块 `s.login_scripts.remove(&tab_id)` 让重连重跑。
2. `note_shell_integration_evidence` 从清空改为**冻结**；`replayable_ops` 不按 `shell_integrated` 过滤。
3. `schedule_cd_after_restore` 等 sender+Connected+静默（非盲睡）；`build_restore_cd_command(cwd)` 共用；`schedule_replay_after_reconnect` 带 `follow_up_cwd`（回放完追发 cd）。

## restore_sessions SSH/Telnet 分支优先级（阶段 5 语义）
1. `should_schedule_replay` 为真（ops 非空 && 脚本无凭据）→ 压制脚本 → 回放 ops（+follow-up cd）。
2. 否则有 login_script → 脚本拥有建立流程，跳过 cd/回放。
3. 纯集成 shell → `schedule_cd_after_restore`。
`reconnect_session` 同构（follow_up_cwd 取当前 tab.cwd）。

## 核心机制（沿用）
- `SessionReplayRecorder { ops, shell_integrated }`，`AppState.session_replays`（serde-skip）；仅 Ssh/Telnet；`REPLAY_MAX_OPS=10`（未冻结且满窗拒录）；凭据提示行不录。
- 回放引擎：等 Connected（60s）→ `filter_replayable_ops`（只回 Safe）→ i18n 提示 → 逐条等静默（4×200ms/20s 超时）→ `{op}\r` → follow-up cd。
- 持久化：`PersistedSession.replay_ops`（serde default）；restore 重播种 `shell_integrated: false`（落到目标后 evidence 再冻结）。**bincode 非自描述，勿加字段**。
- 生命周期：断线保留；close 删除。

## 测试（rusterm-ui 695 passed，rusterm-core 186）
- state.rs `session_replay_tests`：`shell_evidence_freezes_the_establishment_prefix`、`menu_reentry_thaws_and_restarts_recording`、`prompt_classification_separates_menus_from_shells`（阶段 5 新/改）。
- app.rs `session_replay_engine_tests`：`recorded_ops_override_pure_navigation_scripts_but_not_credential_ones`、`credential_detection_looks_for_send_onekey_steps`（阶段 5 改/新）、`restore_cd_command_quotes_paths`。
- strip_prompt 测试：+`cjk_bastion_menu_prompt_strips_fullwidth_colon`。

## 用户环境实证（诊断用）
- jumpserver `jump.zs.shaipower.online`（齐治堡垒机，中文菜单），连接配置了**纯导航** login_script（expect 资产分类列表 → send /cao → 11 → 2，无 send_onekey）→ 阶段 5 后：一旦用户手动换过机器（录到 ops），恢复时脚本被压制、回放最后选择；从未手动导航（ops 空）时脚本照跑。
- 日志 JSON lines `~/Library/Application Support/rusterm/logs/`（UTC）；关键标签 `[REPLAY]`/`[LOGIN-SCRIPT]`/`[RESTORE]`。

## 已知接受的劣化 / 未来工作
- 单键（无 Enter）菜单交互不录；fish/nu 远端普通命令可能被录为菜单 op（安全过滤兜底）；less 的 `/pattern` 等可能污染一条 op。
- 嵌套 ssh、bare-Enter 分页确认不覆盖；静默检测可升级为 OSC 133;A。

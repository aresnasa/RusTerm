# RusTerm — 交互式会话操作录制 + 恢复回放 (2026-08-04)

## 目标
让会话恢复（断线重连 `reconnect_session` + 启动恢复 `restore_sessions`）能"真的恢复" jumpserver 这类交互式 SSH 堡垒机会话：记录建立会话状态的交互输入（菜单导航 + 上下文命令如 `sudo -i`），恢复时按序回放。

提交链：`d1b4648`（阶段1）→ `51e8089`（阶段2 启动恢复弹框）→ `1bb56de`（阶段3 Cmd+Q 弹框）→ `ded0d49`（阶段4 三根因）→ `61a55f3`（阶段5 最后一次选择语义）→ **`68bcd55` + `3669e34`（阶段6+7：上下文命令录制、回显驱动回放节奏、DuckDB 时序事件日志，本文以此为准）**。注意 `68bcd55` 是外部并发进程在任务中途替我提交的（混入了它自己的 OneKey habit 工作），`3669e34` 补上 restore 的 DuckDB 优先逻辑。

## 阶段 7：上下文命令（sudo -i 等）录制回放
1. **`is_context_command(cmd)`**（state.rs 纯函数）：识别"建立持久会话上下文"的命令——`su`、`ssh`、`telnet` 永真；`sudo`/`doas` 仅当带 `-i/--login/-s/--shell` 或目标命令是 shell/su（`-u root` 之类带参 flag 被正确跳过；`sudo systemctl restart` 一次性命令为 false）；`docker/podman/nerdctl/kubectl/oc` + `exec/attach`。
2. **`record_context_command`**（state.rs）：shell 提示符路径的录制入口。冻结态下**追加**（不 thaw、不清菜单前缀）——与 `record_replay_op` 的"菜单重入清空重录"语义并存。同时置 `shell_integrated = true`（直连目标无 OSC 133 时也冻结）。守卫：仅 SSH/Telnet、REPLAY_MAX_OPS 封顶。
3. **`pop_context_command`**：`exit`/`logout`（`is_context_exit_command`）弹掉**尾部**上下文命令（sudo -i → 干活 → exit 后不再回放 sudo）。仅冻结态、仅尾部 op 是上下文命令时生效——目标机上的 exit（回堡垒机菜单）不会吃掉导航前缀。
4. **app.rs on_input Enter 分支**：shell_like 路径现在是 freeze → try record_context_command → else try pop_context_command；菜单路径录制前先记 `was_frozen`（thaw 时写 DuckDB "reset" 事件）。录制日志升 **info** 且带 op 内容。
5. **密码不回放**：回放循环每个 op 发送前 + follow-up cd 前，`credential_kind(extract_current_line())` 检测到凭据提示 → `notify_replay_paused_for_credentials`（i18n `session.replay_paused_credential`）终止自动回放，用户手动输密码。sudo NOPASSWD 则全自动通过。

## 阶段 6：回放顺序修复 + DuckDB 时序层
1. **顺序错乱根因修复（回显驱动节奏）**：原 `wait_for_output_quiescence` 在刚发完 op 时终端本来就静默 → 立即误判发下一条 → 远端错序消费。新增 `wait_for_output_response(state, sid, baseline)`：发 op 前记录 tab.version 为 baseline，下一条 op 前**先等 version 变化（远端回显了）再等静默**。follow-up cd 同样处理。
2. **DuckDB `replay_events` 表**（rusterm-analytics，`CREATE TABLE IF NOT EXISTS` 幂等）：`(connection_id, ts_micros BIGINT, seq BIGINT, event VARCHAR, op VARCHAR)`，索引 `(connection_id, ts_micros, seq)`。**事件溯源语法**：`op`/`context` push、`pop` 弹尾、`reset`（菜单重入 thaw）清空；`fold_replay_events`（pub 纯函数）按 (ts_micros, seq) 折叠出当前 ops；未知事件类型忽略（向前兼容）。`clear()` 已加 DELETE replay_events。
3. **顺序保证**：`persist_replay_event`（app.rs）在 Enter 处理路径**同步**捕获 ts_micros（SystemTime）+ 全局 `REPLAY_EVENT_SEQ: AtomicU64` tiebreaker，仅 DB insert 在 spawn 的异步任务里跑——写入乱序落库不影响折叠顺序。按 **connection_id** 键（session UUID 每次重连都换）。op 文本过 `sanitize_command`，秘密样式整条事件丢弃。
4. **UI 包装**：`AnalyticsHandle::record_replay_event`/`latest_replay_ops` enabled/disabled 双路径（disabled 返回 Ok(())/空 vec，默认构建零成本降级到快照 ops）。
5. **restore 优先级**：`restore_sessions` 先 `analytics.latest_replay_ops(connection_id)`，非空则胜过 `ps.replay_ops`（快照 2s debounce 可能丢尾部 op）；空/错误回退快照。`reconnect_session` 仍用内存 recorder（本来就准确）。**多 tab 同连接的劣化**：DuckDB 按连接键，最后写入者赢（快照按 session 键）——已接受。
6. **构建要求**：DuckDB 层只在 `--features analytics` 构建生效（`cargo build --features analytics`，加 ~50MB）；默认构建为 no-op stub，行为 = 阶段 5 + 上下文命令录制 + 回放节奏修复（这三样不依赖 analytics）。

## 阶段 5 沿用语义（`61a55f3`）
- `prompt_looks_like_shell` 分类：shell 提示符冻结 / 菜单提示符录制；裸 `"> "` 算菜单（JMS Opt> 优先）。
- 菜单重入 thaw：`record_replay_op` 遇冻结 clear+重录（快照永远持最后一次导航）。
- `should_schedule_replay`：ops 非空且脚本无 send_onekey 时回放赢，`suppress_login_script` 压制纯导航脚本。
- `strip_prompt` 支持全角冒号 `：`。

## 核心机制（沿用）
- `SessionReplayRecorder { ops, shell_integrated }`，serde-skip；仅 Ssh/Telnet；REPLAY_MAX_OPS=10；凭据提示行不录。
- 回放引擎：等 Connected（60s）→ `filter_replayable_ops`（安全过滤：sudo -i/su/ssh/docker exec 全部 Safe 放行，reboot/shutdown/rm -rf / 等 Warn 跳过——有回归测试）→ i18n 提示 → 逐条【等上条回显→等静默→查凭据提示→发 `{op}\r`】→ follow-up cd（同样回显+静默+凭据守卫）。
- `PersistedSession.replay_ops`（serde default）不变，**bincode 勿加字段**。

## 测试（rusterm-ui 712 passed 默认 / 711 analytics feature；rusterm-core 186；rusterm-analytics 73）
- state.rs 新增：`context_command_classifier_separates_context_from_oneshot`、`context_commands_append_to_the_frozen_establishment_log`、`context_commands_respect_recording_gates`、`exit_pops_the_trailing_context_command_but_not_the_navigation_prefix`。
- app.rs 新增：`filter_keeps_context_commands_replayable`（sudo -i/su/ssh/docker exec 过安全过滤，sudo reboot 被跳过）。
- analytics lib.rs 新增：`replay_events_fold_in_submission_order`、`replay_events_out_of_order_inserts_still_fold_by_timestamp`、`replay_events_reset_starts_a_fresh_recording`、`replay_events_are_sanitized_and_clearable`。

## 用户环境实证
- jumpserver `jump.zs.shaipower.online`（齐治，中文菜单全角冒号），纯导航 login_script。阶段 5 后 jumpserver 登录恢复已由用户确认工作。
- 日志 `~/Library/Application Support/rusterm/logs/`（UTC JSON lines），`[REPLAY]` 录制现在是 info 级带 op 内容。

## 已知接受的劣化 / 未来工作
- 单键（无 Enter）菜单交互不录；回放中途凭据提示会终止剩余 op（sudo 通常是最后一条，影响小）；replay_events 表无修剪（增长极慢，每菜单/上下文命令一行）。
- 嵌套 ssh 的密码/hostkey 交互不覆盖（回放 ssh 后若要密码 → 凭据守卫暂停）。
- 外部并发进程会改写+提交工作树（本阶段实际发生：`68bcd55` 混入了 OneKey habit 工作）。

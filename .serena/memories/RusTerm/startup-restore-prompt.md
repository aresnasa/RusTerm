# RusTerm — 启动恢复确认对话框（阶段 2，2026-08-04）

前置：阶段 1（交互式会话操作录制 + 恢复回放）见 `mem:RusTerm/session-replay-recovery`，提交 `d1b4648`。
本阶段提交：`51e8089 "feat: startup restore confirmation dialog for normal and crash exits"`。

## 目标与契约
启动时**无论上次正常退出还是异常退出**，只要磁盘上存在非空会话快照就弹出恢复确认框；用户点「恢复」→ `restore_sessions`（含阶段 1 的 replay_ops 回放）；点「跳过」→ 空白开始。旧行为是解锁后自动恢复（无对话框）。

- **异常退出的快照来源**：已有的 30s 周期保存循环（app.rs `_session_state_save_future`）——无需新增机制。
- **正常退出的快照来源**：close 路径的 `save_session_state_snapshot`（两处调用：fast-close + 确认关闭）。

## 实现（复用旧版遗留件，未新建状态字段/组件）
- `AppState.restore_pending: Option<SessionState>`（state.rs，serde-skip）——旧版确认框的字段被**重新启用**，文档已改写。解锁路径置位，恢复/跳过清空。
- `components/restore_session_dialog.rs` **重写**：
  - 新 props：`session_count, saved_at, sessions: Vec<RestoreSessionSummary>, on_restore, on_skip`
  - 新导出结构 `RestoreSessionSummary { name, detail, has_replay }`（components.rs 一并导出）
  - 会话清单区（每行：名称 + "SSH · host" 详情 + 有 replay_ops 时的「回放」徽章）
  - **删除了「不再询问」按钮**（restore_disabled 是被刻意忽略的 legacy，按钮无意义；用户要求总是弹框）
- app.rs：
  - `restore_prompt_items(&SessionState) -> Vec<RestoreSessionSummary>` 纯函数（位于 `startup_restore_candidate` 之后），kind 映射 SSH/Telnet/Serial/Shell/TCP，hostname 非空时拼 "kind · host"
  - 解锁路径（unlock 的 use_future 内）：`startup_restore_candidate` 命中 → `s.restore_pending = Some(to_restore)`，不再直接 `restore_sessions`
  - rsx 前预计算 `restore_prompt: Option<(usize, String, Vec<...>)>`（saved_at 用 `with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")`）；rsx 内 `if let` 渲染，位于 close-confirmation modal 之前
  - on_restore：`state.write().restore_pending.take()` → `restore_sessions(state, input_senders, snapshot)`
- **保存守卫（关键）**：`restore_pending.is_some()` 期间跳过快照保存（30s 周期循环 `continue`；`save_session_state_snapshot` 提前 `return`）——否则弹框未决时当前空白会话列表会覆盖磁盘上待恢复的快照（用户 >30s 未答、或带着弹框退出都会丢数据）。恢复/跳过清空 pending 后保存恢复正常（跳过后下次周期保存覆盖旧快照 = 跳过即放弃，符合「跳过（开始空白会话）」语义）。
- i18n：`restore.skip_hint` 改写（去掉"不再询问"提法）、新增 `restore.will_replay`（"✓ 交互式会话（如堡垒机）将回放已记录的建立操作"）、`restore.replay_badge`（"回放"/"replay"）。

## 测试
- 改名：`legacy_never_ask_flag_does_not_block_startup_restore_prompt`、`empty_snapshot_does_not_trigger_startup_restore_prompt`（函数级契约不变：非空快照→Some 候选，legacy 标志被忽略）
- 新增：`restore_prompt_items_summarize_kind_host_and_replay`
- 全量 rusterm-ui 689 passed；rustfmt 触碰文件干净。

## 注意
- `startup_restore_candidate` 的 `_legacy_restore_disabled` 参数仍被刻意忽略（快照成员资格即"是否该问"的真源）。
- 外部并发进程仍会改写工作树——本任务期间未遇冲突，提交时用 `git commit -- <files>` 只收本任务文件。

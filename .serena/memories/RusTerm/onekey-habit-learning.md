# OneKey 习惯学习层 (Task #111, 2026-08-04)

## 目标
会话使用不同 OneKey 时按用户习惯自动切换，避免每次输入密码/弹窗。弹窗只在必要时出现：出错（凭证被拒）、用户新增/删除了匹配该提示的 OneKey、习惯不明确。用户行为记录到本地 DuckDB。

## 改造前的痛点（弹窗为何每次都出现）
- `take_onekey_selection` 只在 **多匹配 + 安全提示**（`onekey_prompt_is_safe_to_remember`）时生成持久化 `OneKeyPreference` → 单匹配提示每次登录都弹窗。
- 泛型提示（裸 `Password:`）永远不安全 → 永远弹窗。
- 已有 preference 会无条件自动提交，即使用户刚新增了一个也匹配该提示的 OneKey（用户希望此时被重新询问）。

## 两层决策（`apply_onekey_popup`，app.rs ~L7815）
顺序：attempt 失配清理 → repeated-prompt（拒绝）→ **候选集变更门** → Tier1 → Tier2 → 弹窗。
- **Tier 1（原有）**：持久化 `OneKeyPreference`（settings.json，仅安全提示、多匹配、一次选择即记住）。不变。
- **Tier 2（新增，习惯层）**：键 = `(connection_id, habit_fingerprint)`。`habit_fingerprint` 是 `OneKeyPopupState` 新字段——对**每个**提示都计算 `onekey_prompt_fingerprint`（安全提示时与 `prompt_fingerprint` 相同）。纯函数 `resolve_onekey_habit(events, current_candidates_hash)` 返回 `(onekey_id, step_id)` 当且仅当：
  1. 最近两次选择（manual/auto）目标相同；
  2. 两者之后没有更新的 rejection；
  3. 最新选择的 `candidates_hash` == 当前提示的候选集哈希。
  → 拒绝后必须**连续两次人工确认**才恢复自动提交；auto_submit 事件可延续习惯但不能建立习惯。
- **候选集变更门**：`onekey_candidates_hash(matches)` = 排序后 (onekey_id\0step_id) 的 SHA-256。`onekey_candidates_changed` 比较缓存中最新选择事件的哈希与当前哈希，不同 → 弹窗一次（覆盖 Tier1/Tier2）；用户再选一次即重建基线。无历史事件时门不触发（老用户 Tier1 行为不变）。
- 共享提交路径 `submit_remembered_onekey(...)`（提取自原 Tier1 内联代码）：成功→ feedback/cooldown/attempt + AutoSubmit 事件 + sudo lease 缓存；失败→ 弹窗。

## 行为事件（隐私：仅元数据）
`OneKeyBehaviorEvent { connection_id, prompt_fingerprint(哈希), onekey_id, step_id, kind, candidates_hash }`，`OneKeyBehaviorKind::{ManualSelect, AutoSubmit, Rejected, PopupShown}`（`as_str`/`parse` 与 DuckDB action 列互转）。**绝不含**凭证值、显示名、原始提示文本。

## 数据流
- **内存缓存**（同步、可单测）：`AppState.onekey_habit_events: HashMap<(conn,fp), Vec<Event>>`（oldest-first，`ONEKEY_HABIT_EVENTS_CAP=32`/键）+ `AppState.onekey_pending_analytics: Vec<Event>`（待刷队列）。均 serde-skip。
  - `state.rs::record_onekey_behavior`（live：入缓存+入队；PopupShown 只入队不入缓存）
  - `state.rs::seed_onekey_habit_event`（warm-load 回放：只入缓存）
- **DuckDB**（rusterm-analytics）：表 `onekey_events(connection_id, prompt_fingerprint, onekey_id, step_id, action, candidates_hash, created_at)`；`AnalyticsDB::record_onekey_event` / `recent_onekey_events(limit)`（取最新 N 条按时间升序返回）；`clear()` 也清该表。
- **Handle**（rusterm-ui/analytics.rs）：共享结构 `OneKeyUsageRecord`；enabled 实现转发 DuckDB，disabled stub 空操作 → **feature off 时习惯层仍工作（仅进程内，不跨重启）**。
- **刷写**：`app.rs::flush_onekey_behavior_events(Signal)` — read 判空（避免每输出块写 Signal 触发重渲染）→ take 队列 → `spawn` 逐条写 DuckDB。调用点：`check_onekey_match` Ok 分支末尾、手动选择 handler。
- **warm-load**：unlock 成功后（`drop(s)` 前 clone handle）`spawn` `recent_onekey_usage(ONEKEY_HABIT_LOAD_LIMIT=512)` → `seed_onekey_habit_event` 回放。日志 `[ONEKEY-HABIT] warmed habit cache: N events across M prompts`。

## 关键接线改动
- `OneKeySelection` 新增 `behavior: Option<OneKeyBehaviorEvent>`（有 connection+habit_fp 即生成，含单匹配/泛型提示）。
- 手动选择 handler（`on_onekey_select` Ok 分支）：**总是**插入 `onekey_preference_attempts`（合成 preference，含 habit_fp）→ repeated prompt 时能记录 Rejected；`record_onekey_behavior(ManualSelect)`；之后 flush。（旧行为只在多匹配安全提示时插 attempt。）
- attempt 失配比较（apply_onekey_popup 顶部）从 `popup.prompt_fingerprint` 改为 `popup.habit_fingerprint`（安全提示两者相同，语义不变；泛型提示的 attempt 不再被误清）。
- repeated-prompt 分支：移除 attempt 时记录 `Rejected` 事件（forget_onekey_preference 对未持久化的合成 preference 是无害 no-op）。
- 弹窗回退路径记录 `PopupShown`（仅分析用）。
- 日志 tag：`[ONEKEY-HABIT]`；auto-submit 日志带 `source=preference|habit`。

## 测试（全绿：rusterm-ui 711 / +analytics 710，rusterm-analytics 73）
- analytics: `onekey_events_roundtrip_oldest_first`、`recent_onekey_events_limit_keeps_the_newest_rows`、`record_onekey_event_stamps_missing_created_at`、`clear_wipes_onekey_events`。
- app.rs `onekey_habit_resolution_tests`（纯函数）：两次同目标才成习惯、auto 只能延续、拒绝阻断直至两次确认、候选集变化阻断、哈希与顺序无关/对 id 敏感。
- app.rs `session_startup_tests`：`generic_prompt_learns_the_habit_after_two_consistent_manual_selections`、`alternating_selections_never_form_a_habit`、`adding_a_new_matching_onekey_reopens_the_chooser_once`、`rejected_habit_submission_falls_back_to_the_chooser`、`manual_selection_produces_a_behavior_event_for_the_chosen_candidate`。测试辅助 `simulate_manual_selection` / `simulate_credential_accepted` 镜像 handler 逻辑。
- state.rs `onekey_behavior_cache_tests`：PopupShown 不入缓存、队列收全部事件、cap 保留最新、seed 不入队。

## 陷阱/约定
- `apply_onekey_popup` 是同步 `&mut AppState`（测试直接调用）→ 里面**不能 spawn**；DuckDB 写走 pending 队列 + 运行时调用方 flush。
- `flush_onekey_behavior_events` 必须先 `state.read()` 判空再 write，否则每个输出块都触发 Signal 写。
- 并发写入者：本次任务期间有外部进程向 app.rs 注入 replay 持久化代码（`persist_replay_event`、`notify_replay_paused_for_credentials`），先出现调用点后出现定义，短暂编译失败属正常——等待其收敛后重跑测试。

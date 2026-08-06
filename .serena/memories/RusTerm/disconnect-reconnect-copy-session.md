# RusTerm — 会话断开/重连 + 顶部标签栏复制会话 (2026-08-04)

## 目标
1. **全局功能-会话支持断开和重连**：用户可主动断开会话（保留标签页/回滚/登录配置），并随时重连。
2. **顶部的会话支持复制会话**：顶部标签栏右键复制会话，所有连接类型的登录逻辑都要支持复制。

## 现状（探索确认）
断开/重连基础设施**已存在**：`SessionConnectionState{Connected,Disconnected,Reconnecting}`、`session_configs: HashMap<String,ConnectionConfig>`、`reconnect_session()`（支持 SSH/Shell/Telnet/Serial）、断开的终端上 Enter/右键即重连、`begin_reconnect()`（仅允许 Disconnected→Reconnecting）、`RECONNECT_WATCHDOG_SECS=45`。
`open_connection()` 已支持全部 4 种类型（Ssh/Shell/Telnet/Serial）+ Tcp fallback，并存储 `session_configs[tab_id]=conn`。

**缺失**：① 无主动"断开"操作；② 顶部标签栏无右键菜单/无"复制会话"；③ `open_local_terminal` 未存 `session_configs`，本地终端无法重连/复制。

## 改动

### i18n (`i18n.rs`)
新增 4 键：`session.disconnect`(断开连接)、`session.reconnect`(重新连接)、`session.copy_session`(复制会话)、`session.disconnected_by_user`(已由用户断开)。

### `disconnect_session_state` + `disconnect_session` (`app.rs`)
- `disconnect_session_state(&mut AppState, &mut input_senders, &str) -> bool`：纯状态函数（可单测，对标 `begin_reconnect`）。镜像 `SessionEvent::Disconnected` handler 的状态变更：置 Disconnected、清 ssh_sessions/sftp_clients/onekey_*/login_scripts/transfers/zmodem、移除 input_sender、`last_command_status=Disconnected(reason)`。**保留** `session_configs`/`terminals`/`sessions`（重连+回滚所需）。Guard：仅 Connected/Reconnecting 可断开，已断开/未知 no-op（幂等）。
- `disconnect_session(Signal, Signal, String)`：Signal 包装。先 `flush_pending_renders(true)`，调 `disconnect_session_state`，发 close 信号终止后台任务，清 close_senders/resize_senders（让重连可装新的），`sync_live_sessions`。

### `open_local_terminal` (`app.rs`)
补存 `session_configs[session_id]`（ShellConfig）+ `session_connection_states=Connected`，使本地终端也支持重连/复制（满足"所有会话的登录逻辑都要支持复制"）。

### TabBar 右键上下文菜单 (`tab_bar.rs`)
- 新增 props：`connection_states: HashMap<String,SessionConnectionState>`、`on_disconnect/on_reconnect/on_copy_session: EventHandler<String>`。
- `context_menu: Signal<Option<(String,f64,f64)>>` + 每个 tab 的 `oncontextmenu`（prevent_default + set signal）。
- 菜单 3 项：⏏断开（仅 Connected/Reconnecting 启用）、⟳重连（仅 Disconnected 启用）、⧉复制会话（始终启用）。镜像 Sidebar 的 context-menu 模式（click-away backdrop + fixed 定位菜单）。
- **dioxus rsx! 约束**：rsx! 内不允许 `let`，`{}` 格式段不接受 `matches!` 宏。解决：在 `rsx!` 之前预读 signal 计算 `menu_state`/`can_disconnect`/`can_reconnect`/`disconnect_style`/`reconnect_style` + 3 个 `menu_sid_*` Option<String> 克隆（move 闭包各取其一，避免 moved value）。

### App 接线 (`app.rs` TabBar 调用处)
- `on_disconnect` → `disconnect_session(state, input_senders, sid)`
- `on_reconnect` → `reconnect_session(state, input_senders, sid)`
- `on_copy_session` → 读 `session_configs[sid]` 克隆，`new_conn.name=tf("connection.copy_name",...)`，`open_connection(state, input_senders, new_conn, None)`（新会话得新 session id，保留原 saved-connection id 使 OneKey/sudo 一致）。无 config 时 warn。

### 复制会话回放登录逻辑 (2026-08-06 补充)
用户反馈"复制会话只克隆了 UI/传输配置，没有真正重放登录进机器"。修复：`on_copy_session` 现在镜像 reconnect 的回放路径——
- 新纯函数 `seed_copied_session_replay(&mut AppState, source_id, new_id) -> Vec<String>`（app.rs，紧跟 `suppress_login_script` 之后）：读 `replayable_ops(source)`，非空则把 `SessionReplayRecorder { ops, shell_integrated:false }` 种到新会话 id 下（副本自己的后续 reconnect 也能回放，镜像 restore_sessions 的 re-seed），返回 ops。
- handler 流程：捕获源 tab 的 `cwd`（follow_up）→ `open_connection` 返回 `OpenConnectionResult.session_id` → `seed_copied_session_replay` → `should_schedule_replay(login_script, ops)` 为真时：非空脚本先 `suppress_login_script(new_sid)`（防双驱菜单），然后 `schedule_replay_after_reconnect(state, input_senders, new_sid, ops, follow_up_cwd)`。该函数等待 Connecting→Connected（对全新连接同样适用），安全过滤 + 回显节奏 + 凭据守卫全部沿用。
- 日志：`[COPY-SESSION] copy <new> of <src> scheduling N recorded op(s) for post-connect replay`。
- 测试（session_startup_tests，2 新）：`copy_session_seeds_replay_recorder_and_schedules_login_replay`（继承 jumpserver 导航+sudo -i 序列、副本 recorder 被种、源不受影响、纯导航脚本被 ops 压制、凭据脚本仍赢）、`copy_session_without_recorded_ops_seeds_nothing`（普通 SSH 复制行为不变）。ui lib 772 全绿。

### 副本就近放置（任务 127，2026-08-06）
用户要求"副本支持就近复制，不需要放到最右边"。`open_connection` 天然 append 到最右。修复：
- 新纯函数 `place_copied_session_next_to_source(&mut AppState, source_sid, copy_sid) -> bool`（state.rs，紧跟 `reorder_tab` 之后）：① 找源 session 的 workspace tab，复用 `reorder_tab(copy_sid, source_tab_id, before=false)` 把副本 tab 移到源 tab 右侧（reorder_tab 保 active tab —— 副本保持激活）；② 同步把 `state.sessions` 里的副本条目移到源条目之后 —— `build_session_state` 按 sessions 顺序落快照，因此**重启恢复后副本也仍然紧邻源会话**。No-op：源/副本 id 未知、source==copy、源无 tab（pane-only）。复制最右 tab 时内部 reorder_tab 返回 false（已相邻）但函数仍返回 true。
- app.rs `on_copy_session`：`open_connection` 返回后立刻调 `place_copied_session_next_to_source(&mut state.write(), &sid, &new_sid)`，再走 seed/replay 流程（顺序无耦合）。
- 恢复逻辑复查：阶段 9（见 `mem:RusTerm/session-replay-recovery`）的 connection_id 匹配 + 副本流种子 + 名字保留仍完好，`restore_resolves_copied_sessions_by_connection_id_and_keeps_title` 等测试全绿，无需再改。
- 测试（state.rs tests，4 新 + `append_copy` helper）：`copied_session_is_placed_immediately_after_its_source`（tabs+sessions 双重顺序 + active 保持）、`copy_of_rightmost_tab_is_already_adjacent`、`place_copied_session_with_unknown_source_is_noop`、`place_copied_session_with_unknown_copy_is_noop`。ui lib 782 全绿。
- 注意：`move_session_to_leftmost`（SSH 首次登录 per-host autoconfig）只在 host 从未配置过时移动 —— 副本的 host 必然已配置，不会打架。

## 测试 (`session_startup_tests`，6 新，全绿)
- `disconnect_sets_state_to_disconnected_and_preserves_config_and_tab`：断开后状态=Disconnected、config 保留、tab 保留、badge=Disconnected。
- `disconnect_is_idempotent_for_already_disconnected_session`：重复断开 no-op。
- `disconnect_is_noop_for_unknown_session`：未知 sid no-op。
- `disconnect_then_begin_reconnect_allows_retry`：断开后 begin_reconnect 接受（Disconnected→Reconnecting）。
- `copy_session_clones_full_login_logic_across_all_kinds`：4 种 kind（SSH/Shell/Telnet/Serial）的 config 克隆保留 id/kind/group/tags/onekey/login_script，仅 name 变为副本。

## 验证
- `cargo build -p rusterm-app`：clean。
- `cargo test -p rusterm-ui --lib`：678 passed, 0 failed。
- nightly rustfmt：clean。

## 注意
- 外部进程持续重写+提交工作树。本次功能代码已被外部进程提交进 HEAD（`331d8a5 "add diag"` 等）。唯一未提交的是 2 行 Theme API 修复（`*theme ==` → `matches!(theme,)`，解 blocked 的预存在 skin 编译错误，非本任务代码）。
- `rusterm-core` 有 1 个预存在失败测试 `palette_custom_uses_the_resolved_variant_slot`（外部进程 skin 工作，非本任务）。

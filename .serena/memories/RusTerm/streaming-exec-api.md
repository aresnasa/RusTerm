# RusTerm — 流式 exec API（/api/v1/exec/stream，2026-08-04, commit `5aaf62d`）

## 动机
复杂/长时间查询命令走 API 时，旧实现阻塞到命令执行完才一次性返回（`execute_request` await `executor.exec` 全量结果），造成传递时延。用户要求：数据分块 + 流式输出（内存缓冲队列喂流式响应）。

## 服务端（rusterm-relay）
- **`POST /api/v1/exec/stream`**（server.rs）：请求体与 `/api/v1/exec` 完全相同（ExecRequest：command XOR script XOR script_base64、elevated、timeout_ms）。响应 `application/x-ndjson` chunked：
  - `{"event":"chunk","data":"..."}`（到达即发）
  - 终结事件恰好一个：`{"event":"done","exit_code":..,"timed_out":..,"truncated":..,"duration_ms":..}` 或 `{"event":"error","message":"..."}`（dispatch 后结局不可知，如 live PTY 中途关闭；此时 HTTP 已 200，失败只能带内）。
  - dispatch **前**的失败（认证/校验/host 授权/未知主机）仍是普通 JSON 错误响应 + 真实状态码。
- **`prepare_exec` 抽取**：认证→payload 解析→限流→校验(+script sandbox)→host 授权→timeout clamp→accepted 审计，返回 `PreparedExec{account_username,payload,is_script,host,exec_selector,timeout}` 或 `Box<Response>`。buffered（/exec、/r/{host}）与 streaming 两路共用，行为不能漂移。错误映射同样抽成 `exec_error_response`，完成记账抽成 `log_exec_completion`（审计 ok + record_history）。
- **NDJSON 流**：`futures::stream::unfold(Option<ExecStreamBodyState>)` → `axum::body::Body::from_stream`；Done/Failed/通道意外关闭时在流内做审计+历史记账后结束。relay Cargo 新增 `futures.workspace = true`。

## Executor trait（executor.rs）
- `pub enum ExecStreamEvent { Chunk(String), Done{exit_code,timed_out,truncated,duration_ms}, Failed{message} }`（PTY 路径 stderr 并入 stdout；buffered 回退 stdout/stderr 各一 chunk）。
- `RelayExecutor::exec_stream(...) -> Result<mpsc::Receiver<ExecStreamEvent>, ExecutorError>`，**默认实现**= await `exec()` 后经 `buffered_exec_stream(outcome)`（cap-4 channel，try_send ≤3 事件）回放——所有 executor 天然兼容端点。
- 导出：`ExecStreamEvent`、`buffered_exec_stream`。

## App 层真流式（rusterm-ui/relay_tunnel.rs）
- `AppRelayExecutor::exec_stream` 覆盖：仅 **非 elevated + find_live_session 命中** 走真流式 `exec_stream_via_live_session`；BeforeSend 错误按 bastion_live_only 决定拒绝 or 回退 buffered `self.exec`；其余路径（elevated sudo、fresh connection、bastion 守卫）全部回退 buffered（安全逻辑复用）。
- `exec_stream_via_live_session(entry, command, timeout)`：与 buffered 相同的 sentinel 包裹（`{ cmd ; } ; __rc=$? ; printf '\n{TAG}%d\n' "$__rc"`），发送后立即返回 `Receiver`；spawn 任务持 tap+deadline：**`mpsc::channel(64)` 有界通道就是 PTY 读取与慢 HTTP 客户端之间的缓冲（backpressure 暂停 tap 循环而非涨内存）**。超时→mark_relay_exec_unusable + flush + Done{timed_out}；tap 关闭→Failed；客户端挂断（send Err）→静默停止转发（命令在 PTY 里继续跑）。
- **`LiveExecStreamParser`（纯函数式增量解析器，可单测）**：`feed(&[u8]) -> (Option<String>, Option<u32>)`；
  - 完整行立刻放行；尾部残行 pending（防 marker 半截泄漏）；
  - 含 `__rc=$?` 或 rc_tag 的行丢弃（镜像 strip_echoed_wrapper；回显 wrapper 行同时含两者）；
  - `find_complete_rc_marker` 命中→放行 marker 前内容 + 返回 exit code；
  - 无换行超 `STREAM_PENDING_FLUSH_THRESHOLD=64KiB` → flush（保留 rc_tag.len()+32 尾巴；marker 自带前导 \n 所以安全）；
  - 总量 cap `MAX_STREAM_EXEC_OUTPUT=32MiB`：超出置 truncated、抑制输出但继续扫 marker（char boundary 安全截断）；
  - `flush()` 供超时路径清尾。

## 测试
- relay server.rs +4：`exec_stream_endpoint_relays_chunks_then_done`（ScriptedStreamExecutor 分页发事件 + history 记账断言）、`..._buffered_fallback_via_default_impl`、`..._keeps_pre_dispatch_error_statuses`（403/404/401）、`..._reports_in_band_failure_as_error_event`。测试助手 `raw_stream_request` + `dechunk`（裸 TCP，手动解 chunked）。relay 120。
- relay_tunnel.rs +6 parser 测试（完整行/残行、回显 wrapper 丢弃、跨 read 分裂 marker 不泄漏、flush、cap+marker、巨型无换行 flush）。ui 744。

## 客户端消费
`curl -sN -u user:pass -H 'content-type: application/json' -d '{"host_id":"...","command":"..."}' http://.../api/v1/exec/stream`，逐行 JSON。旧端点 `/api/v1/exec`、`/r/{host}` 契约完全不变（`~/.agents/skills/rusterm` 的脚本无需改动；可选后续让 skill 的 raw 用 stream 端点降低时延）。

## 注意
- 流式响应 headers 先行 → dispatch 后失败无法改状态码，只能带内 error 事件；客户端必须检查最后一行 event。
- `ExecStreamBodyState` unfold 状态 `Option<...>`：终结事件产出 `(line, None)` 结束流。
- 真流式只覆盖 live-PTY 非提权路径；fresh-connection/sudo 路径仍是"结束后一次性回放"（rusterm-ssh 内部 exec 本身是 buffered，未来可下钻）。
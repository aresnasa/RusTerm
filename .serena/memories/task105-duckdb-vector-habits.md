# DuckDB 向量存储 + 用户习惯记忆层 (Task #105)

## 目标
优化用户操作逻辑记录：在 DuckDB 中存储命令的向量嵌入（embedding），按使用概率（recency-weighted 频率）排序，建模用户习惯与记忆，用于预测性建议。

## 实现位置
- `crates/rusterm-analytics/src/embed.rs` (新增) — 嵌入器
- `crates/rusterm-analytics/src/lib.rs` (扩展) — 向量存储表 + 衰减排名 + 混合 top-K 查询
- `crates/rusterm-ui/src/analytics.rs` (扩展) — UI 层包装（enabled + disabled 双路径）

## 设计决策
1. **嵌入策略**：默认用 `HashEmbedder`（特征哈希 + 字符 3-gram，128 维，L2 归一化），零依赖、纯本地、确定性（FNV-1a 哈希）。`Embedder` trait 留有扩展点，未来可接入 candle 神经嵌入器。
   - 不用神经模型的原因：严格 local-only 安全策略 + 现有 candle 树过重（1.5GB）。对 shell 命令，token + 字符 n-gram 已能捕获语义相似度（含拼写容错）。
2. **存储**：`command_embeddings(command PK, embedding VARCHAR)`，embedding 存为 JSON 数组字符串。向量数学（cosine）在 Rust 端做，避免 duckdb-rs 数组类型绑定问题；衰减频率在 SQL 端算（单次扫描 <1ms）。
3. **衰减评分**：`decayed_score = Σ daily_decay ^ age_days`（默认 0.99 ≈ 69 天半衰期）。查询时计算，无漂移。
4. **混合 top-K**：`score = alpha * max(0, cosine_sim) + (1-alpha) * (decayed/max_decayed)`，默认 alpha=0.5。负相似度贡献 0（不相关命令只靠频率）。
5. **隐私**：嵌入基于 sanitized 命令计算；PEM/密钥等被 sanitizer 丢弃的命令不记录、不嵌入。
6. **UI 层**：`record_command` 现在自动嵌入（live 路径预热缓存）；`suggest_by_context(partial, limit)` 暴露给调用方；`backfill_embeddings` 用于 mirror 后补全历史命令的嵌入。

## 公共 API
### rusterm-analytics
- `HashEmbedder::new()` / `Embedder` trait / `cosine_similarity(a,b)`
- `AnalyticsDB::record_command_embedded(cmd, &dyn Embedder)` — 记录 + 缓存嵌入
- `AnalyticsDB::upsert_embedding(command, &[f32])` — 直接写缓存
- `AnalyticsDB::backfill_embeddings(&dyn Embedder) -> u64` — 补全缺失嵌入
- `AnalyticsDB::habit_rankings(daily_decay, limit) -> Vec<HabitRanking>` — 衰减频率排名
- `AnalyticsDB::suggest_by_context(&[f32], &SuggestOptions) -> Vec<HabitSuggestion>` — 混合 top-K
- 类型：`HabitRanking`, `HabitSuggestion`, `SuggestOptions`（含 Default）

### rusterm-ui (AnalyticsHandle)
- `suggest_by_context(partial, limit) -> Vec<String>`（enabled 用默认 SuggestOptions；disabled 返回空）
- `backfill_embeddings() -> u64`（disabled 返回 0）
- `record_command` 现在自动嵌入

## 验证
- `cargo test -p rusterm-analytics`：65 测试全过（含 11 个新增习惯记忆测试）
- `cargo test -p rusterm-ui --features analytics`：677 测试全过
- `cargo test -p rusterm-ui`（默认）：678 测试全过
- `cargo check --workspace`：通过
- `cargo clippy -p rusterm-analytics`：无新警告

## 未做（后续工作）
- **接入实时建议管道**：把 `suggest_by_context` 接到 keystroke 触发的建议 UI（state.rs/app.rs），需要 UX 决策（何时触发、与现有 SQLite frecency 建议如何合并）。
- **candle 神经嵌入器**：trait 已就绪，可在 `rusterm-ai` 加 `Embedder` 实现接 MiniLM。
- **mirror 后自动 backfill**：`mirror_from_sqlite` 完成后调用 `backfill_embeddings`，目前需手动调。

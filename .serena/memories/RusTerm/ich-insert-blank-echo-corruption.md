# 终端渲染 bug：命令回显部分消失（ICH 修复，2026-08-04, `7c203c4`）

## 症状
远程 shell 中，提示符后已输入命令的前半段变成空白，只剩尾部残缺文本（如 `du -sh /var/lib/containerd/` 只显示 `h /var/lib/containerd/`），输出正常。

## 根因
`crates/rusterm-core/src/terminal.rs` 的 `Handler::insert_blank`（ICH，`CSI n @`）右移循环写错范围：

```rust
// BUG：目标索引一路降到 col
for i in (col..cols).rev() {
    if i >= count { row.cells[i] = std::mem::take(&mut row.cells[i - count]); }
```

最后 `count` 步执行 `cells[col..col+count] = take(cells[col-count..col])`——**读取并清空光标左侧的单元格**。readline 在行中插入字符的回显是每键一对 `CSI @` + 字符（典型：光标回退后在已有文本前补打命令），于是每插入一个字符就吞掉前一个刚插入的字符 → 前缀全部变空白，只剩最后一个字符 + 原尾部文本。

## 修复
目标从 `col + count` 起（与 `write_char` insert-mode 分支对称）：
```rust
for i in (col + count..cols).rev() {
    row.cells[i] = std::mem::take(&mut row.cells[i - count]);
}
```
`col+count > cols` 时 range 为空，安全。

## 回归测试（terminal.rs::tests）
- `insert_blank_preserves_cells_left_of_cursor`：直接 `CSI 2 @`，光标左侧提示符必须完好。
- `readline_mid_line_insertion_keeps_previously_typed_prefix`：模拟 readline 逐字符 `CSI @`+char 在 `h /var/lib/containerd/` 前插入 `du -s`，最终行必须是完整命令。

## 排查时排除的方向（勿重查）
- 建议弹窗（纯 DOM overlay，不写 grid）；`row_to_html` 的 suggestion stop_at 截断只影响光标行且纯视觉；replay 落库不碰渲染；`render_with_scroll` 每帧全量快照无缓存；`erase_chars`/`delete_chars`/`put_tab`/`move_forward`/`clear_line`/`resize+reflow` 均正确。
- 同类移位代码检查过：`delete_chars`（正确）、`write_char` insert-mode（正确）、`insert_blank_lines`/`delete_lines`（行级，正确）。

## 测试基线（此提交后）
rusterm-core lib **195**（+2），rusterm-ui 744，rusterm-relay 120，rusterm-analytics 74。

## 教训
- 提交信息含反引号/括号会被 sh 当替换执行 → 用 `--` 或引号内避免反引号。
- 外部自动提交进程又抢先了一次（`3686929 fix terminal` 混入 .claude/Claude.md）→ `git reset --soft` + 只 add 自己的文件重提为 `7c203c4`。

# Feishu OTP CDP 搜索链路根因与修复（2026-08-07 验证）

## 根因
飞书 Web 搜索用 Slate 编辑器。`execCommand('insertText')`（即使包 compositionstart/end synthetic events）和 synthetic beforeinput/input 都**只更新 DOM，不触发 Slate 搜索** → 候选永不出现 → "搜不到智小安"。DOM read-back 会假阳性（文本在但搜索没跑）。
**唯一可靠方式 = CDP `Input.dispatchKeyEvent` + `Input.insertText` 逐字符**（trusted，走 Slate 真实输入管线），前置条件：
1. `Runtime.enable` + evaluate 激活 target
2. 真实 mouse click（pressed+released，无需 mouseMoved）落在 palette editor 上
3. click 后 sleep 300ms 让 palette settle
4. JS `el.focus()`（过 `searchEditorHasFocus` gate）
5. 清空：`Input.dispatchKeyEvent keyDown commands:["selectAll"]` + keyUp + Backspace
6. 逐字符 insertText，间隔 30ms
→ `.bot-result-card > .bot-chatter-info-name` 候选 ~1s 内出现；click 卡片中心即跳进会话（composer placeholder 变 "发送给 智小安"）。

## 2026-08-07 第二轮修复（composer clear-failed + 回复体提取 + Enter 进会话）
真实日志暴露三个新 bug，均已修（cargo check/test 42 绿，真实页面只读验证）：
1. **composer "could not clear editor"**：Slate placeholder（`.editor__custom--placeholder-content`，在 `data-void`/`contenteditable=false` 子树里）**仍被 editor.innerText 读出** → 清空后 read-back 永远 = "发送给 智小安" → clear-failed。修复：`element_text_script` 先剔除 `[data-void], .editor__custom--placeholder` 子树文本再 trim。
2. **点卡片不进会话**：`click(.bot-result-card)` 只弹机器人资料面板 → composer 校验超时（"未能确认名称和发送框均匹配目标机器人"）。修复：候选校验（exact name + 机器人徽章 + 唯一）保留为 gate，之后 **`press_enter()` 进会话**（实测 Enter 打开聊天本身）。
3. **机器人回复 body 为空**：bot 回复是 universal card，文本不在 `.message-content` 下（如 `回复 徐超: \n动态口令\notp：096512，有效期剩余：42秒`）→ 轮询永远拿不到回复。修复：`message_snapshots` body 提取降级链 `.message-content` → `.universal-card` → wrapper，取第一个非空 innerText。
- 回复格式全角冒号 `otp：096512`，`parse_otp_reply` 的 `otp[：:]\s*(\d{6})` 已兼容；默认 pattern `\b\d{4,8}\b` 也能中（42秒 只 2 位不匹配）。
- 会话名元素 `.chatWindow_chatName` === "智小安"；composer 校验读 `.editor__custom--placeholder-content` = "发送给 智小安" 均已在真实 DOM 确认。

## 其他实测事实
- `.open-app-card`（SERP 应用 tab 卡片）click **不会**触发任何导航（事件到达但无 handler）——不要走 SERP 路径
- composer `[contenteditable=true].innerdocbody` 同样适用 insertText；selectAll 在占位符状态下首次可能静默失败（selectAll 无内容可选无害）
- CDP Input 坐标 = CSS pixels（getBoundingClientRect），window 1920x917 时 1:1
- 调试时 CDP 端口读 `~/Library/Application Support/rusterm/feishu-browser/chrome/DevToolsActivePort` 第一行；target id 会变，每次从 /json/list 取
- execute_python 环境是 keycloak conda env，可 `pip install websocket-client`

## 代码改动
`crates/rusterm-ui/src/feishu_browser.rs`：`type_into_editor` 重写为【JS focus → native clear → 逐字符 Input.insertText(30ms) → read-back】，4 次重试；删除 `text_insert_script`（execCommand 管线全废）。`automate_feishu_otp` 里 click search_editor 后加 300ms settle。
验证：cargo check 绿，cargo test -p rusterm-ui feishu 42/42。已在真实 Chrome 上用 python 复刻完全相同的 rust 步骤序列端到端通过（进入智小安会话）。

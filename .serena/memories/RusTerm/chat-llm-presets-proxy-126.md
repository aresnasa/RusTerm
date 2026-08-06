# Chat LLM round-trip + provider presets + proxy (issue #126, 2026-08-06)

## Root cause of "配置了 API Key 仍报尚未接入 LLM"
Two compounding gaps in the #122-era chat panel:
1. `render_agent_config`'s Save button read `draft_api_key` ONLY to build a
   feedback string — the key was **discarded**, never stored anywhere.
2. `send_message` was a stub: 300ms sleep + `chat.stub_reply` placeholder
   ("尚未接入 LLM——请配置智能体与 API Key…"). It never called any client.

## Fix architecture
- **`AppState.chat_api_keys: HashMap<String, String>`** (serde-skip, in
  `state.rs`) — in-memory API keys keyed by agent id. Save button inserts
  the trimmed draft key. Never persisted (project secret policy).
  Also added `chat_request_in_flight: bool` to block double-sends.
- **`rusterm-ai/src/chat.rs`** (new): `ChatProtocol { OpenAiCompatible,
  Anthropic }`, `ChatTurn/ChatTurnRole`, `ChatRequest`, `complete_chat()`
  (multi-turn, 60s timeout, error bodies truncated to 400 chars at char
  boundary), `ProxySelection { System | Disabled | Url }`,
  `build_http_client(proxy)`, `detect_clash_proxy()` (probes loopback
  7890 http / 7897 http (Clash Verge) / 7891 socks5, 300ms per port).
- **`rusterm-ai/src/presets.rs`** (new): `ProviderPreset` +
  `builtin_presets()` — 13 entries: OpenAI, Anthropic, Gemini (OpenAI-compat
  endpoint), Kimi, xAI (closed); DeepSeek, Qwen/DashScope, GLM, SiliconFlow,
  Groq, OpenRouter (open); Ollama + LM Studio (local, `requires_key: false`).
  `merge_presets(builtin, remote)` replaces by id in place, appends new.
  `fetch_remote_presets(REMOTE_PRESETS_URL, proxy)` — URL points at
  `raw.githubusercontent.com/aresnasa/RusTerm/main/assets/llm-presets.json`.
  **`assets/llm-presets.json` is pinned in sync with the builtin catalog by
  test `shipped_remote_catalog_file_matches_builtin_presets`** — editing one
  requires editing the other.
- **`ChatSettings` new persisted fields** (config.rs, all serde-default):
  `allow_remote_presets: bool` (explicit network consent, default FALSE — the
  online refresh button is disabled until the user checks the consent box),
  `proxy_mode: ChatProxyMode { System(default) | Off | Clash | Custom }`,
  `proxy_url: String`. Note: ChatSettings has NO `..Default::default()`
  usage in tests — literal constructors in config.rs tests must list every
  field.
- **`send_message` (chat_panel.rs)** now: preflight (no agent / key missing
  for non-local non-loopback providers / model empty → System bubble with
  actionable text `chat.need_api_key`/`chat.need_model`), builds turns from
  User/Assistant messages (System bubbles are UI notices, excluded from
  model context), resolves proxy via `resolve_proxy_selection` (Clash mode
  probes at send time, falls back to Disabled), calls `complete_chat`.
  Provider mapping: OpenAI/Local → OpenAiCompatible protocol, Anthropic →
  Anthropic. Local provider and loopback base URLs skip the key preflight.
- **Agent config popover additions**: preset `<select>` (applies name/model/
  base_url/provider to drafts on change + shows key-acquisition URL hint),
  consent checkbox (persists immediately), "在线更新" button (consent-gated,
  fetch+merge+status), proxy mode select + custom URL input (persist
  immediately via on_save_chat). New signal `draft_provider` seeded on ⚙
  open so applying a preset can switch protocol.
- **reqwest workspace features += "socks"** (socks5:// proxy support).
- i18n: `chat.stub_reply` REMOVED; added `chat.need_api_key`, `chat.need_model`,
  `chat.request_failed`, `chat.request_in_flight`, `chat.api_key_saved`,
  `chat.preset_*`, `chat.network_consent`, `chat.proxy_*` (EN+ZH).

## Consent + privacy invariants
- No network fetch of presets EVER happens unless
  `chat_settings.allow_remote_presets == true` (checkbox in popover; the
  refresh handler double-checks it as belt-and-braces).
- LLM requests only fire on explicit user send. API keys live only in
  `chat_api_keys` (memory), never logged, never persisted.

## Tests
- rusterm-ai: 28 (9 chat + 6 presets new).
- rusterm-ui `chat_config_tests` in chat_panel.rs: proxy mode round-trip,
  loopback detection, resolve_proxy_selection mapping (tokio::test), and
  api-key-store lookup contract.
- Workspace all green (one unrelated flaky failure observed once in a
  parallel run, did not reproduce).
- Committed as `e534a6a` (2026-08-06). Note: user screenshots live in
  `.claude/image_*.png` and can be read as images.

## Future work
- Route API keys through macOS Keychain (like OneKey credentials).
- Streaming responses (current call is buffered; reqwest `stream` feature
  is already enabled).
- Read Clash config yaml (`~/.config/clash/config.yaml` mixed-port) instead
  of fixed-port probing.

# Qwen2.5-Coder-1.5B Local Inference Integration

## Architecture

Integrated HuggingFace `Qwen/Qwen2.5-Coder-1.5B-Instruct` for offline template
generation in the API panel. Uses [candle](https://github.com/huggingface/candle)
0.11.0 for local inference with Rust-side quantization.

## Key Design Decisions

1. **Feature-gated** (`qwen-local` in rusterm-ai, propagated via rusterm-ui).
   Off by default — candle deps are heavy and the model needs ~1.5 GB RAM.
   Sub-features: `qwen-local-metal` (macOS GPU), `qwen-local-cuda` (NVIDIA GPU).

2. **Rust-side quantization**: Downloads original BF16 safetensors from HF Hub,
   quantizes to Q4_K GGUF using `candle::quantized::QTensor::quantize` +
   `gguf_file::write`. Cached at `<data_dir>/RusTerm/qwen-local/`. The 3 GB
   safetensors is deleted after quantization; only the ~1 GB GGUF is kept.

3. **tensor-tools does NOT do name mapping**: candle's `tensor-tools` quantize
   command does a 1:1 tensor copy with empty metadata. We implemented the
   HF→GGUF name mapping ourselves (see `map_tensor_name` in qwen_local.rs).

4. **GGUF metadata**: `quantized_qwen2::ModelWeights::from_gguf` reads these
   keys: `qwen2.attention.head_count`, `qwen2.attention.head_count_kv`,
   `qwen2.embedding_length`, `qwen2.context_length`, `qwen2.block_count`,
   `qwen2.attention.layer_norm_rms_epsilon`, `qwen2.rope.freq_base`.

5. **Hardware detection**: `detect_hardware()` checks CPU cores, RAM, Metal/CUDA.
   Min: 4 cores, 4 GB RAM. Warning shown in settings but user can force-enable.

6. **UI**: Settings dialog has a toggle. API panel has "AI Generate" button with
   Shell/Python toggle and natural-language description input. Generation runs
   on a background thread; UI polls via mpsc channel + tokio::task::yield_now.

## Phase 2: hf-mirror + Custom Models (issue #98)

1. **Mirror URL**: `download_file` uses `hf_hub::api::sync::ApiBuilder::new()
   .with_endpoint(mirror_url)` instead of `Api::new()`. Default mirror is
   `https://hf-mirror.com` (stored in `QwenLocalSettings.mirror_url`).
   Users can change to `https://huggingface.co` in the settings UI.

2. **Custom models**: `ModelConfig` struct (in `rusterm-core::config`) holds
   id/name/repo_id/architecture/prompt_template/eos_token. `builtin_models()`
   returns 3 qwen2 presets (1.5B, 0.5B, Qwen2-1.5B). `resolve_model()` finds
   the active model by id (builtins first, then custom, then fallback).
   `QwenLocalSettings` now has `mirror_url`, `active_model_id`, `custom_models`.

3. **Multi-file safetensors**: `download_safetensors()` auto-detects single vs
   multi-file by trying to download `model.safetensors.index.json`. If found,
   parses `weight_map` and downloads all unique shards. `quantize_to_gguf()`
   accepts `&[PathBuf]` and merges tensors from all shards.

4. **Parameterized APIs**: `ensure_model(cache_dir, &ModelConfig, mirror_url, progress)`,
   `QwenLocalModel::load(gguf_path, tokenizer_path, &ModelConfig)`. The model's
   `prompt_template` and `eos_token` are used at load + generate time. GGUF
   cache filename derived from model id: `{id}-q4k.gguf`.

5. **Architecture validation**: Only `"qwen2"` is supported (checked early in
   `ensure_model`). Other architectures return a clear error before downloading.

6. **Settings UI**: SettingsDialog now receives full `QwenLocalSettings` (not
   just `enabled: bool`). Adds mirror URL input, model selector dropdown
   (builtins + custom), collapsible "Add custom model" form, and custom model
   list with delete buttons.

## Files Changed

- `Cargo.toml` — workspace deps: candle (renamed from candle-core), candle-nn,
  candle-transformers, tokenizers, hf-hub, rayon
- `crates/rusterm-ai/Cargo.toml` — `qwen-local` feature + optional deps
- `crates/rusterm-ai/src/lib.rs` — module exports (cfg-gated)
- `crates/rusterm-ai/src/qwen_local.rs` — core: hardware detect, download,
  quantize, load, generate (680+ lines)
- `crates/rusterm-ai/src/template_gen.rs` — prompt engineering + response parsing
- `crates/rusterm-core/src/config.rs` — `QwenLocalSettings` in PersistedConfig
- `crates/rusterm-core/src/config_manager.rs` — load/save methods
- `crates/rusterm-ui/Cargo.toml` — `qwen-local` feature passthrough + base64 dep
- `crates/rusterm-ui/src/components/api_panel.rs` — AI generate UI
- `crates/rusterm-ui/src/components/settings_dialog.rs` — settings toggle
- `crates/rusterm-ui/src/app.rs` — SettingsDialog wiring
- `crates/rusterm-ui/src/i18n.rs` — `ai_runtime.local.*` keys (EN+ZH)

## Build Commands

```sh
# Default (no local AI):
cargo build --release

# With local AI (CPU only):
cargo build --release -p rusterm-app --features rusterm-ui/qwen-local

# With Metal (macOS):
cargo build --release -p rusterm-app --features rusterm-ui/qwen-local-metal
```

## Pitfalls

- Dioxus rsx! does NOT support `#[cfg]` attributes — use feature-gated
  helper functions with `#[cfg]` on the function itself, not inside rsx.
- `Signal` is `Copy` in Dioxus 0.7 — no `mut` needed to call `.set()`.
- rayon `par_iter().map()` requires `Send + Sync` closures — use `Fn` (not
  `FnMut`) + `Sync` bound for progress callbacks, or use atomics.
- candle workspace renames `candle-core` → `candle` in workspace deps.
- `gguf_file::write` takes `&[(&str, &Value)]` and `&[(&str, &QTensor)]`.
- **rsx! format strings**: `{prompt}` in a `placeholder` attr is parsed as a
  format placeholder → use `{{prompt}}` to escape. But `t("...{prompt}...")`
  is fine because it returns `&str` at runtime, not a format string.
- **hf-hub 0.5.0 mirror API**: `ApiBuilder::new().with_endpoint(String)` —
  NOT `Api::new()` (which hardcodes huggingface.co). `ApiBuilder::from_env()`
  also reads `HF_ENDPOINT` env var.
- **render_ai_section** is a standalone function — `state` is NOT in scope.
  Pass settings as a parameter from `ApiPanel` (which has `state`).

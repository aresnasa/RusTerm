//! Local on-device LLM inference for template generation.
//!
//! Uses [candle](https://github.com/huggingface/candle) to run a quantized
//! **Qwen2.5-Coder-1.5B-Instruct** model entirely on the user's machine —
//! no API keys, no network calls after the initial download.
//!
//! # Pipeline
//!
//! 1. **Download** the original BF16 safetensors from HuggingFace
//!    (`Qwen/Qwen2.5-Coder-1.5B-Instruct`).
//! 2. **Quantize** on the Rust side using `candle::quantized::QTensor::quantize`
//!    to GGUF Q4_K format (4-bit K-quants — the recommended balance of size
//!    and quality for a 1.5B model). The quantized GGUF is cached so this
//!    expensive step only happens once.
//! 3. **Load** the GGUF via `quantized_qwen2::ModelWeights::from_gguf` and
//!    run autoregressive generation with `LogitsProcessor`.
//!
//! # Hardware gating
//!
//! A 1.5B Q4_K model needs ~1 GB for weights + ~100 MB for KV cache. The
//! feature is opt-in and [`detect_hardware`] reports whether the host can
//! reasonably run it so the UI can show a friendly warning on low-spec
//! machines.

#![cfg(feature = "qwen-local")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use candle::Device;
use candle::quantized::gguf_file::{self, Value};
use candle::quantized::{GgmlDType, QTensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
use rayon::prelude::*;
use tokenizers::Tokenizer;

// ── Constants ───────────────────────────────────────────────────────────

/// HuggingFace repo for the original (unquantized) model.
const HF_REPO: &str = "Qwen/Qwen2.5-Coder-1.5B-Instruct";

/// Quantization level applied during the Rust-side conversion.
/// Q4_K offers ~4-bit precision with K-quant super-blocks — the
/// recommended default for 1-2B models per llama.cpp benchmarks.
const QUANT_DTYPE: GgmlDType = GgmlDType::Q4K;

/// Qwen2 instruct chat template tokens.
const PROMPT_TEMPLATE: &str = "<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n";
const EOS_TOKEN: &str = "<|im_end|>";

/// Inference defaults — tuned for short template generation.
const DEFAULT_TEMPERATURE: f64 = 0.7;
const DEFAULT_TOP_P: f64 = 0.9;
const DEFAULT_MAX_TOKENS: usize = 512;
const DEFAULT_REPEAT_PENALTY: f32 = 1.1;

// ── Hardware detection ─────────────────────────────────────────────────

/// Snapshot of the host's ability to run the local model.
///
/// Returned by [`detect_hardware`] so the UI can decide whether to offer
/// the "AI Generate" button or show a "your machine may be too slow"
/// warning.
#[derive(Debug, Clone)]
pub struct HardwareCapability {
    /// Logical CPU cores visible to the process.
    pub cpu_cores: usize,
    /// Total physical RAM in MB, if detectable on this platform.
    pub total_memory_mb: Option<u64>,
    /// Whether Apple Metal GPU acceleration is available.
    pub has_metal: bool,
    /// Whether NVIDIA CUDA GPU acceleration is available.
    pub has_cuda: bool,
    /// Whether the host meets the minimum bar to run the 1.5B model.
    pub can_run: bool,
    /// Human-readable caveat shown to the user when `can_run` is false or
    /// marginal. Empty string when everything looks good.
    pub warning: String,
}

/// Minimum CPU cores for a usable experience (below this, generation is
/// painfully slow — single-digit tokens/second).
const MIN_CPU_CORES: usize = 4;
/// Minimum total RAM. The Q4_K model is ~1 GB; we want headroom for the OS
/// and the rest of the app.
const MIN_MEMORY_MB: u64 = 4_096;

/// Probe the host machine and decide whether the local model can run.
///
/// This is a *heuristic* — it does not guarantee performance, only that
/// the machine isn't obviously too small. The user can still force-enable
/// the feature even when `can_run` is false; the UI just warns them.
pub fn detect_hardware() -> HardwareCapability {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let has_metal = candle::utils::metal_is_available();
    let has_cuda = candle::utils::cuda_is_available();

    let total_memory_mb = total_memory_mb();

    let mut warnings = Vec::new();

    if cpu_cores < MIN_CPU_CORES {
        warnings.push(format!(
            "CPU cores ({cpu_cores}) below recommended minimum ({MIN_CPU_CORES}); generation will be very slow."
        ));
    }

    if let Some(mem) = total_memory_mb {
        if mem < MIN_MEMORY_MB {
            warnings.push(format!(
                "System RAM ({mem} MB) below recommended minimum ({MIN_MEMORY_MB} MB); the model may not load."
            ));
        }
    }

    // Without a GPU, a 1.5B Q4 model runs on CPU — usable but not fast.
    // We still allow it; the warning is about speed, not feasibility.
    if !has_metal && !has_cuda && cpu_cores < MIN_CPU_CORES {
        warnings.push(
            "No GPU acceleration detected and CPU is below minimum; \
             consider keeping this feature disabled."
                .to_string(),
        );
    }

    let can_run = total_memory_mb.map(|m| m >= MIN_MEMORY_MB).unwrap_or(true) && cpu_cores >= 2; // absolute floor: 2 cores

    HardwareCapability {
        cpu_cores,
        total_memory_mb,
        has_metal,
        has_cuda,
        can_run,
        warning: warnings.join(" "),
    }
}

/// Best-effort total system RAM in MB. Returns `None` if the platform-
/// specific lookup fails (we never block the feature on this).
#[cfg(target_os = "macos")]
fn total_memory_mb() -> Option<u64> {
    // sysctl hw.memsize returns bytes.
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let bytes: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(bytes / (1024 * 1024))
}

#[cfg(target_os = "linux")]
fn total_memory_mb() -> Option<u64> {
    let output = std::process::Command::new("cat")
        .arg("/proc/meminfo")
        .output()
        .ok()?;
    let info = String::from_utf8_lossy(&output.stdout);
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn total_memory_mb() -> Option<u64> {
    // Use the Windows API via wmic (deprecated but widely available).
    let output = std::process::Command::new("wmic")
        .args(["ComputerSystem", "get", "TotalPhysicalMemory", "/value"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("TotalPhysicalMemory=") {
            let bytes: u64 = rest.trim().parse().ok()?;
            return Some(bytes / (1024 * 1024));
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn total_memory_mb() -> Option<u64> {
    None
}

// ── Device selection ───────────────────────────────────────────────────

/// Pick the best available compute device. Mirrors candle-examples' logic
/// but without depending on the `candle-examples` crate (which is not
/// published to crates.io as a library).
fn best_device() -> Result<Device> {
    if candle::utils::cuda_is_available() {
        Device::new_cuda(0).map_err(|e| anyhow!("CUDA init failed: {e}"))
    } else if candle::utils::metal_is_available() {
        Device::new_metal(0).map_err(|e| anyhow!("Metal init failed: {e}"))
    } else {
        Ok(Device::Cpu)
    }
}

// ── HF config parsing ──────────────────────────────────────────────────

/// Subset of HuggingFace `config.json` fields needed to build the GGUF
/// metadata and drive the quantizer. Extra fields are ignored.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // parsed for completeness; not all fields are used
struct HfConfig {
    vocab_size: usize,
    hidden_size: usize,
    #[serde(default)]
    intermediate_size: usize,
    num_hidden_layers: usize,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    max_position_embeddings: usize,
    #[serde(default = "default_rms_eps")]
    rms_norm_eps: f64,
    #[serde(default = "default_rope_theta")]
    rope_theta: f64,
    #[serde(default)]
    tie_word_embeddings: bool,
}

fn default_rms_eps() -> f64 {
    1e-6
}
fn default_rope_theta() -> f64 {
    1_000_000.0
}

// ── Tensor name mapping (HF safetensors → GGUF) ────────────────────────

/// Map a HuggingFace safetensors tensor name to the GGUF naming convention
/// expected by `quantized_qwen2::ModelWeights::from_gguf`.
///
/// Qwen2.5-Coder uses the `qwen2` architecture, so the mapping is:
///
/// | HF name                                  | GGUF name                  |
/// |------------------------------------------|----------------------------|
/// | `model.embed_tokens.weight`              | `token_embd.weight`        |
/// | `model.norm.weight`                      | `output_norm.weight`       |
/// | `lm_head.weight`                         | `output.weight`            |
/// | `model.layers.{i}.self_attn.q_proj.weight` | `blk.{i}.attn_q.weight`  |
/// | `model.layers.{i}.self_attn.q_proj.bias`   | `blk.{i}.attn_q.bias`    |
/// | ... (k/v/o_proj, mlp gate/down/up, norms)  | ...                      |
///
/// Returns `None` for unrecognized tensors (they are skipped).
fn map_tensor_name(hf_name: &str) -> Option<String> {
    // Global tensors.
    match hf_name {
        "model.embed_tokens.weight" => return Some("token_embd.weight".to_string()),
        "model.norm.weight" => return Some("output_norm.weight".to_string()),
        "lm_head.weight" => return Some("output.weight".to_string()),
        _ => {}
    }

    // Per-layer tensors: model.layers.{i}.{suffix}
    let rest = hf_name.strip_prefix("model.layers.")?;
    let (idx_str, suffix) = rest.split_once('.')?;
    let layer_idx: usize = idx_str.parse().ok()?;

    let gguf_suffix = match suffix {
        "self_attn.q_proj.weight" => "attn_q.weight",
        "self_attn.q_proj.bias" => "attn_q.bias",
        "self_attn.k_proj.weight" => "attn_k.weight",
        "self_attn.k_proj.bias" => "attn_k.bias",
        "self_attn.v_proj.weight" => "attn_v.weight",
        "self_attn.v_proj.bias" => "attn_v.bias",
        "self_attn.o_proj.weight" => "attn_output.weight",
        "mlp.gate_proj.weight" => "ffn_gate.weight",
        "mlp.down_proj.weight" => "ffn_down.weight",
        "mlp.up_proj.weight" => "ffn_up.weight",
        "input_layernorm.weight" => "attn_norm.weight",
        "post_attention_layernorm.weight" => "ffn_norm.weight",
        _ => return None,
    };

    Some(format!("blk.{layer_idx}.{gguf_suffix}"))
}

/// Build the GGUF metadata key-value pairs that
/// `quantized_qwen2::ModelWeights::from_gguf` reads.
fn build_gguf_metadata(cfg: &HfConfig) -> Vec<(&'static str, Value)> {
    vec![
        (
            "qwen2.attention.head_count",
            Value::U32(cfg.num_attention_heads as u32),
        ),
        (
            "qwen2.attention.head_count_kv",
            Value::U32(cfg.num_key_value_heads as u32),
        ),
        ("qwen2.embedding_length", Value::U32(cfg.hidden_size as u32)),
        (
            "qwen2.context_length",
            Value::U32(cfg.max_position_embeddings as u32),
        ),
        (
            "qwen2.block_count",
            Value::U32(cfg.num_hidden_layers as u32),
        ),
        (
            "qwen2.attention.layer_norm_rms_epsilon",
            Value::F32(cfg.rms_norm_eps as f32),
        ),
        ("qwen2.rope.freq_base", Value::F32(cfg.rope_theta as f32)),
    ]
}

// ── Quantization ───────────────────────────────────────────────────────

/// Progress callback fired during model setup.
#[derive(Debug, Clone)]
pub enum SetupProgress {
    /// Downloading a file from HuggingFace. `progress` is 0.0–1.0.
    Downloading { file: String, progress: f64 },
    /// Quantizing tensors. `current`/`total` index the tensor batch.
    Quantizing {
        current: usize,
        total: usize,
        tensor: String,
    },
    /// All done — the GGUF is ready at the cached path.
    Done,
}

/// Decide the quantization dtype for a single tensor.
///
/// - `output.weight` (lm_head) uses Q6K for better output quality, matching
///   llama.cpp / candle tensor-tools behavior.
/// - Other 2D weight matrices use the global Q4K.
/// - Biases and RMSNorm weights stay F32 (they're tiny and quantizing them
///   hurts quality disproportionately).
fn dtype_for(gguf_name: &str, tensor_rank: usize) -> GgmlDType {
    if gguf_name == "output.weight" {
        return GgmlDType::Q6K;
    }
    let is_weight = gguf_name.ends_with(".weight");
    let is_2d = tensor_rank == 2;
    if is_weight && is_2d {
        QUANT_DTYPE
    } else {
        GgmlDType::F32
    }
}

/// Quantize a directory of BF16 safetensors into a single Q4_K GGUF file.
///
/// This is the "Rust-side quantization" step: we load the original
/// HuggingFace safetensors, remap tensor names to the GGUF convention,
/// quantize weight matrices with `QTensor::quantize`, and write the
/// result with `gguf_file::write`. The output is a standard GGUF that
/// `quantized_qwen2::ModelWeights::from_gguf` can load directly.
///
/// `progress` is called as each tensor is quantized so the UI can show a
/// progress bar.
fn quantize_to_gguf(
    safetensors_path: &Path,
    config_path: &Path,
    output_path: &Path,
    progress: &(impl Fn(SetupProgress) + Sync),
) -> Result<()> {
    // 1. Parse config.json for the metadata fields.
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config.json at {config_path:?}"))?;
    let hf_config: HfConfig = serde_json::from_str(&config_text).context("parsing config.json")?;

    let metadata = build_gguf_metadata(&hf_config);

    // 2. Load all tensors from the safetensors file.
    let tensors: HashMap<String, candle::Tensor> =
        candle::safetensors::load(safetensors_path, &Device::Cpu)
            .with_context(|| format!("loading safetensors at {safetensors_path:?}"))?;

    // 3. Map names + quantize in parallel (rayon).
    //    Progress is tracked via an atomic counter because rayon's
    //    parallel map requires `Send + Sync` closures — a `&mut FnMut`
    //    callback doesn't satisfy that.
    let total = tensors.len();
    let indexed: Vec<(String, candle::Tensor)> = tensors.into_iter().collect();
    let counter = std::sync::atomic::AtomicUsize::new(0);

    let qtensors: Vec<(String, QTensor)> = indexed
        .par_iter()
        .map(|(hf_name, tensor)| {
            let gguf_name = map_tensor_name(hf_name).unwrap_or_else(|| {
                // Keep unrecognized tensors under their original name so
                // nothing is silently dropped. They'll be ignored by
                // from_gguf if not needed.
                hf_name.to_string()
            });
            let dtype = dtype_for(&gguf_name, tensor.rank());

            let qtensor = QTensor::quantize(tensor, dtype)
                .map_err(|e| anyhow!("quantizing {gguf_name}: {e}"))?;

            let i = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            progress(SetupProgress::Quantizing {
                current: i + 1,
                total,
                tensor: gguf_name.clone(),
            });

            Ok((gguf_name, qtensor))
        })
        .collect::<Result<Vec<_>>>()?;

    // 4. Write the GGUF file.
    let refs: Vec<(&str, &QTensor)> = qtensors
        .iter()
        .map(|(name, qt)| (name.as_str(), qt))
        .collect();
    let meta_refs: Vec<(&str, &Value)> = metadata.iter().map(|(k, v)| (*k, v)).collect();

    let mut file = std::fs::File::create(output_path)
        .with_context(|| format!("creating output GGUF at {output_path:?}"))?;
    gguf_file::write(&mut file, &meta_refs, &refs).context("writing GGUF file")?;

    progress(SetupProgress::Done);
    Ok(())
}

// ── Model download + cache management ──────────────────────────────────

/// Filenames we need from the HuggingFace repo.
const SAFETENSORS_FILE: &str = "model.safetensors";
const TOKENIZER_FILE: &str = "tokenizer.json";
const CONFIG_FILE: &str = "config.json";
const GGUF_FILE: &str = "qwen25-coder-1.5b-q4k.gguf";

/// Ensure the quantized model exists in `cache_dir`. Downloads + quantizes
/// on first run; subsequent calls return the cached GGUF path immediately.
///
/// **Expensive** — must be called from a background thread, not the UI
/// thread. The `progress` callback is invoked throughout so the UI can
/// show a progress bar / status text.
pub fn ensure_model(cache_dir: &Path, progress: impl Fn(SetupProgress) + Sync) -> Result<PathBuf> {
    let progress = progress;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating cache dir {cache_dir:?}"))?;

    let gguf_path = cache_dir.join(GGUF_FILE);
    let tokenizer_path = cache_dir.join(TOKENIZER_FILE);
    let config_path = cache_dir.join(CONFIG_FILE);
    let safetensors_path = cache_dir.join(SAFETENSORS_FILE);

    // Fast path: GGUF already cached.
    if gguf_path.exists() && tokenizer_path.exists() {
        return Ok(gguf_path);
    }

    // Download tokenizer + config (small files, always needed).
    download_file(TOKENIZER_FILE, &tokenizer_path, &progress)?;
    download_file(CONFIG_FILE, &config_path, &progress)?;

    // Download the BF16 safetensors (large, ~3 GB) if not already present.
    if !safetensors_path.exists() {
        download_file(SAFETENSORS_FILE, &safetensors_path, &progress)?;
    }

    // Quantize BF16 → Q4_K GGUF.
    quantize_to_gguf(&safetensors_path, &config_path, &gguf_path, &progress)?;

    // Clean up the 3 GB safetensors to save disk space. The GGUF is the
    // permanent cache; if the user wants to re-quantize at a different
    // level, the safetensors will be re-downloaded.
    let _ = std::fs::remove_file(&safetensors_path);

    Ok(gguf_path)
}

/// Download a single file from the HuggingFace repo into `dest`.
///
/// Uses `hf_hub`'s sync API with a simple progress estimate based on
/// file size. The hf_hub crate caches downloads in its own internal
/// cache, so repeated calls are cheap.
fn download_file(
    filename: &str,
    dest: &Path,
    progress: &(impl Fn(SetupProgress) + Sync),
) -> Result<()> {
    let api = hf_hub::api::sync::Api::new().map_err(|e| anyhow!("hf_hub init: {e}"))?;
    let repo = api.model(HF_REPO.to_string());
    let downloaded = repo
        .get(filename)
        .map_err(|e| anyhow!("downloading {filename}: {e}"))?;

    // hf_hub caches files in its own dir; copy to our cache_dir for
    // a predictable location.
    if downloaded != dest {
        std::fs::copy(&downloaded, dest)
            .with_context(|| format!("copying {filename} to {dest:?}"))?;
    }

    // Report a coarse 100% — hf_hub's sync API doesn't expose byte-level
    // progress. The UI shows "Downloading {filename}…" which is enough.
    progress(SetupProgress::Downloading {
        file: filename.to_string(),
        progress: 1.0,
    });

    Ok(())
}

// ── Inference ──────────────────────────────────────────────────────────

/// A loaded, ready-to-generate quantized Qwen2.5-Coder model.
///
/// Holds the model weights, tokenizer, and compute device in one struct.
/// Clone is not implemented — there's only one model instance per session.
/// Call [`QwenLocalModel::generate`] for each user prompt; the KV cache is
/// cleared between calls so prompts are independent.
pub struct QwenLocalModel {
    model: Qwen2,
    tokenizer: Tokenizer,
    device: Device,
    eos_token_id: u32,
}

impl QwenLocalModel {
    /// Load a quantized GGUF + tokenizer from disk into memory.
    ///
    /// **Expensive** (~1 GB loaded) — call from a background thread.
    pub fn load(gguf_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let device = best_device()?;

        // Load tokenizer.
        let tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow!("loading tokenizer: {e}"))?;

        // Load GGUF model.
        let mut file = std::fs::File::open(gguf_path)
            .with_context(|| format!("opening GGUF at {gguf_path:?}"))?;
        let content = gguf_file::Content::read(&mut file)
            .map_err(|e| anyhow!("reading GGUF content: {e}"))?;
        let model = Qwen2::from_gguf(content, &mut file, &device)
            .map_err(|e| anyhow!("building Qwen2 model: {e}"))?;

        // Resolve EOS token id.
        let eos_token_id = tokenizer
            .get_vocab(true)
            .get(EOS_TOKEN)
            .copied()
            .ok_or_else(|| anyhow!("EOS token '{EOS_TOKEN}' not in vocab"))?;

        Ok(Self {
            model,
            tokenizer,
            device,
            eos_token_id,
        })
    }

    /// Generate text from a user prompt using the Qwen2 instruct chat
    /// template. Returns the full assistant response (without the prompt).
    ///
    /// Uses nucleus sampling (top-p) with a repeat penalty for natural,
    /// non-repetitive output. The KV cache is cleared before each call so
    /// prompts are independent.
    pub fn generate(&mut self, user_prompt: &str) -> Result<String> {
        self.generate_with_params(
            user_prompt,
            DEFAULT_MAX_TOKENS,
            DEFAULT_TEMPERATURE,
            Some(DEFAULT_TOP_P),
            DEFAULT_REPEAT_PENALTY,
        )
    }

    /// Generate with explicit sampling parameters. Exposed for tests and
    /// advanced UI controls.
    pub fn generate_with_params(
        &mut self,
        user_prompt: &str,
        max_tokens: usize,
        temperature: f64,
        top_p: Option<f64>,
        repeat_penalty: f32,
    ) -> Result<String> {
        // Clear any leftover KV cache from a previous call.
        self.model.clear_kv_cache();

        // Format the prompt with the Qwen2 instruct chat template.
        let prompt = PROMPT_TEMPLATE.replace("{prompt}", user_prompt);

        // Tokenize.
        let encoding = self
            .tokenizer
            .encode(prompt.as_str(), true)
            .map_err(|e| anyhow!("tokenizing prompt: {e}"))?;
        let prompt_tokens: Vec<u32> = encoding.get_ids().to_vec();

        // Set up the logits processor.
        let sampling = if temperature <= 0.0 {
            Sampling::ArgMax
        } else {
            match top_p {
                Some(p) => Sampling::TopP { p, temperature },
                None => Sampling::All { temperature },
            }
        };
        let mut logits_processor = LogitsProcessor::from_sampling(
            299_792_458, // seed — deterministic for reproducible templates
            sampling,
        );

        // ── Prefill: process the entire prompt at once ──────────────
        let mut all_tokens: Vec<u32> = Vec::with_capacity(max_tokens);
        let input = candle::Tensor::new(prompt_tokens.as_slice(), &self.device)?.unsqueeze(0)?;
        let logits = self
            .model
            .forward(&input, 0)
            .map_err(|e| anyhow!("prefill forward: {e}"))?;
        let logits = logits.squeeze(0)?;
        let mut next_token = logits_processor.sample(&logits)?;
        all_tokens.push(next_token);

        // ── Decode: generate one token at a time ────────────────────
        let prompt_len = prompt_tokens.len();
        for index in 0..max_tokens.saturating_sub(1) {
            let input = candle::Tensor::new(&[next_token], &self.device)?.unsqueeze(0)?;
            let logits = self
                .model
                .forward(&input, prompt_len + index)
                .map_err(|e| anyhow!("decode forward: {e}"))?;
            let logits = logits.squeeze(0)?;

            // Apply repeat penalty over the recent context window.
            let logits = if repeat_penalty == 1.0 {
                logits
            } else {
                let last_n = all_tokens.len().min(64);
                let start = all_tokens.len() - last_n;
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    repeat_penalty,
                    &all_tokens[start..],
                )?
            };

            next_token = logits_processor.sample(&logits)?;
            all_tokens.push(next_token);

            if next_token == self.eos_token_id {
                break;
            }
        }

        // Decode the generated tokens back to text.
        let text = self
            .tokenizer
            .decode(&all_tokens, true)
            .map_err(|e| anyhow!("decoding output: {e}"))?;

        Ok(text)
    }
}

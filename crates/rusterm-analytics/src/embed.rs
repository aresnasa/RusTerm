//! Command embedding for habit-memory vector search.
//!
//! Maps a command line to a fixed-dimensional `f32` vector so that DuckDB can
//! rank user operations by *semantic similarity* as well as by frequency.
//! Together with the decayed-frequency ranking in `AnalyticsDB::habit_rankings`
//! / `suggest_by_context`, this is the "user habits and memory" layer: it lets
//! the suggestion pipeline answer "what does the user usually do that looks
//! like *this*?" instead of only "what does the user do most often?".
//!
//! ## Why a hash embedder (not a neural one)?
//!
//! RusTerm's security policy is **strictly local** — no command data may leave
//! the machine. A neural embedder (e.g. MiniLM via candle) would either need a
//! ~90 MB model download or a heavy build-time dependency, and the existing
//! `candle` feature is already gated behind `qwen-local` precisely because of
//! that cost. For *shell commands* (short, token-dense, lots of shared
//! flags/subcommands), a **feature-hashing embedder** with subword n-grams
//! captures practically all the signal:
//!
//! - Word tokens (`git`, `status`, `--verbose`) → command/flag structure.
//! - Character 3-grams → typo tolerance (`gstatus` ≈ `git status`) and sub-word
//!   similarity (`docker` ≈ `dockerd`), fastText-style.
//!
//! The result is a 128-d L2-normalised vector per command. Cosine similarity
//! then approximates "how similar are these two command lines in terms of the
//! tools and flags they invoke". This is local, deterministic, zero-dependency
//! and stable across runs (FNV-1a hash, not `DefaultHasher` which is explicitly
//! non-stable across Rust versions).
//!
//! The [`Embedder`] trait lets a future `candle`-backed neural embedder drop in
//! behind the same `analytics` feature flag without touching call sites.

use std::borrow::Cow;

/// Pluggable command embedder. Implementations must be deterministic (same
/// input ⇒ same output) so that cached embeddings in `command_embeddings`
/// stay valid, and `Send + Sync` so they can live behind the `AnalyticsDB`
/// lock / be shared across tasks.
pub trait Embedder: Send + Sync {
    /// Vector dimensionality. Must be stable for the lifetime of a given
    /// `command_embeddings` table — changing it invalidates cached rows.
    fn dim(&self) -> usize;

    /// Embed a command line into a `dim()`-length `f32` vector. Implementations
    /// should L2-normalise the result so cosine similarity is a plain dot
    /// product.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Default embedder: signed feature hashing ("hashing trick") over word tokens
/// and character 3-grams, L2-normalised.
///
/// Dimensionality defaults to 128, which keeps collision probability low for
/// the <10k unique commands a single user accumulates while staying tiny
/// (128 × 4 bytes = 512 B per cached row, ~5 MB for 10k commands).
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

/// Sensible default: 128 dimensions. Stable — do not change without bumping
/// the `command_embeddings` schema, or cached vectors from an older dim will
/// fail to deserialise / produce wrong cosine scores.
pub const DEFAULT_DIM: usize = 128;

impl Default for HashEmbedder {
    fn default() -> Self {
        Self { dim: DEFAULT_DIM }
    }
}

impl HashEmbedder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a non-default dimension. Almost always you want
    /// [`HashEmbedder::new`] — custom dims are mainly for experiments.
    pub fn with_dim(dim: usize) -> Self {
        assert!(dim > 0, "embedding dimension must be > 0");
        Self { dim }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let dim = self.dim;
        let mut vec = vec![0.0f32; dim];
        let features = tokenize(text);
        for feature in features {
            // FNV-1a is stable across Rust versions (unlike DefaultHasher).
            let h = fnv1a_64(feature.as_bytes());
            let idx = (h % dim as u64) as usize;
            // Signed feature hashing (Weinberger et al. 2009): use the
            // high bit as a sign so collisions cancel in expectation rather
            // than always adding — this roughly halves the bias from
            // collisions and keeps unrelated commands near-orthogonal.
            let sign = if (h >> 63) & 1 == 0 { 1.0f32 } else { -1.0f32 };
            // Word tokens carry more structure than char n-grams, so weight
            // them higher. Char 3-grams are frequent and overlapping, so a
            // smaller weight keeps them from dominating.
            let weight = if feature.contains(' ') || is_word_token(&feature) {
                1.0
            } else {
                0.5
            };
            vec[idx] += sign * weight;
        }
        l2_normalize(&mut vec);
        vec
    }
}

/// Split a command line into embedding features: word tokens plus character
/// 3-grams of the lowercased whole line. Word tokens preserve command/flag
/// structure; char n-grams give typo- and sub-word tolerance.
///
/// Tokens are lowercased so `Git` and `git` hash to the same bucket. Punctuation
/// that carries shell semantics (`|`, `;`, `&`, `=`, `--`) is kept as its own
/// token so e.g. piped commands share the `|` feature.
fn tokenize(text: &str) -> Vec<Cow<'_, str>> {
    let lower = text.to_lowercase();
    let mut out: Vec<Cow<'_, str>> = Vec::new();

    // Word tokens: runs of [a-z0-9._/-] (covers command names, flags, paths,
    // numbers). Everything else is a single-char "structural" token.
    let mut cur = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | '-' | '+' | '~') {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                out.push(Cow::Owned(std::mem::take(&mut cur)));
            }
            if !ch.is_whitespace() {
                // Structural punctuation as its own token (|, ;, &, =, >, <, etc).
                out.push(Cow::Owned(ch.to_string()));
            }
        }
    }
    if !cur.is_empty() {
        out.push(Cow::Owned(cur));
    }

    // Character 3-grams over the lowercased whole line (including spaces) for
    // sub-word / typo similarity. Skip if the line is too short to yield any.
    let chars: Vec<char> = lower.chars().collect();
    if chars.len() >= 3 {
        for window in chars.windows(3) {
            out.push(Cow::Owned(window.iter().collect()));
        }
    }

    out
}

/// True if a token looks like a "word" token (alphanumeric, possibly with
/// `-`/`_`/`/`/`.`) rather than a single-char structural token or a char
/// n-gram (which always has length 3 but may contain spaces).
fn is_word_token(tok: &str) -> bool {
    tok.len() != 1 && !tok.contains(' ') && tok.chars().any(|c| c.is_ascii_alphanumeric())
}

/// L2-normalise in place. A zero vector (empty/no features) is left as-is —
/// cosine similarity with it is defined as 0 by callers.
fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// FNV-1a 64-bit. Stable, dependency-free, good distribution for short keys.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Cosine similarity for L2-normalised vectors = dot product. Falls back to
/// `0.0` for mismatched lengths or zero vectors so callers can treat the result
/// as a non-negative relevance signal without extra guards.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    // Vectors are pre-normalised in the embedder; but the query embedding
    // coming from a caller might not be, so normalise defensively.
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_is_respected() {
        let e = HashEmbedder::new();
        assert_eq!(e.dim(), DEFAULT_DIM);
        let v = e.embed("git status");
        assert_eq!(v.len(), DEFAULT_DIM);
    }

    #[test]
    fn custom_dim() {
        let e = HashEmbedder::with_dim(64);
        assert_eq!(e.dim(), 64);
        assert_eq!(e.embed("ls").len(), 64);
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let e = HashEmbedder::new();
        // Caching in command_embeddings relies on this.
        let a = e.embed("kubectl get pods -n default");
        let b = e.embed("kubectl get pods -n default");
        assert_eq!(a, b);
    }

    #[test]
    fn l2_normalized() {
        let e = HashEmbedder::new();
        let v = e.embed("docker run -it alpine sh");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding must be L2-normalised, got norm = {norm}"
        );
    }

    #[test]
    fn similar_commands_have_higher_similarity_than_unrelated() {
        let e = HashEmbedder::new();
        let git_status = e.embed("git status");
        let git_log = e.embed("git log");
        let docker_ps = e.embed("docker ps");

        let sim_related = cosine_similarity(&git_status, &git_log);
        let sim_unrelated = cosine_similarity(&git_status, &docker_ps);
        // Two git subcommands share the `git` token + several char n-grams,
        // so they must be more similar than git vs docker.
        assert!(
            sim_related > sim_unrelated,
            "git status vs git log ({sim_related}) should beat git status vs docker ps ({sim_unrelated})"
        );
        assert!(sim_related > 0.0, "related commands should be positive");
    }

    #[test]
    fn typo_tolerance_via_char_ngrams() {
        let e = HashEmbedder::new();
        let correct = e.embed("git status");
        let typo = e.embed("gut status"); // one-char typo
        let unrelated = e.embed("kubectl delete deployment");
        let sim_typo = cosine_similarity(&correct, &typo);
        let sim_unrelated = cosine_similarity(&correct, &unrelated);
        assert!(
            sim_typo > sim_unrelated,
            "typo (gut status) should be closer to (git status) than an unrelated command"
        );
    }

    #[test]
    fn case_insensitive() {
        let e = HashEmbedder::new();
        assert_eq!(e.embed("Git Status"), e.embed("git status"));
    }

    #[test]
    fn empty_command_is_zero_vector() {
        let e = HashEmbedder::new();
        let v = e.embed("");
        assert!(v.iter().all(|x| x.abs() < 1e-9));
        // cosine with anything is 0
        assert_eq!(cosine_similarity(&v, &e.embed("ls")), 0.0);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_lengths() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn fnv1a_is_stable() {
        // Pin a few known values so a future refactor can't silently change
        // the hash and invalidate persisted embeddings.
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a_64(b"git"), 0xd50b6318facd7f0b);
    }
}

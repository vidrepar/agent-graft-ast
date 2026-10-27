//! Token counting with the o200k_base BPE (GPT-4o family), loaded once per process.
//! GitHub's `bpe-openai` counter: identical counts to tiktoken on all 2,198 benchmark files, ~1.7x faster.
//!
//! Loading the BPE takes ~50 ms, so the server starts it on a background thread at launch;
//! a receipt written before it is ready uses a byte estimate and says so (`estimated="true"`).

use std::sync::OnceLock;

static BPE: OnceLock<&'static bpe_openai::Tokenizer> = OnceLock::new();

pub const TOKENIZER: &str = "o200k_base";

fn bpe() -> &'static bpe_openai::Tokenizer {
    BPE.get_or_init(|| {
        let t = bpe_openai::o200k_base();
        /* first use builds the tables: do it here so `ready` means ready */
        t.count("warm");
        t
    })
}

/// Exact count (blocks until the tokenizer is loaded).
pub fn count(s: &str) -> usize {
    bpe().count(s)
}

/// True once the tokenizer is loaded (counting is then cheap).
pub fn ready() -> bool {
    BPE.get().is_some()
}

/// Exact count when the tokenizer is ready, otherwise a byte estimate; the flag says which.
pub fn count_fast(s: &str) -> (usize, bool) {
    match BPE.get() {
        Some(b) => (b.count(s), true),
        None => (s.len().div_ceil(BYTES_PER_TOKEN), false),
    }
}

/// o200k averages ~4 bytes per token on source code.
const BYTES_PER_TOKEN: usize = 4;

/// Load the tokenizer now, on this thread.
pub fn warm() {
    let _ = bpe();
}

/// Load the tokenizer on a background thread; returns at once.
pub fn start_loading() {
    std::thread::spawn(warm);
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_tokens() {
        assert_eq!(super::count(""), 0);
        assert!(super::count("pub fn main() {}") >= 4);
        let (n, exact) = super::count_fast("pub fn main() {}");
        assert!(exact && n == super::count("pub fn main() {}"));
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

pub struct TokenMetrics {
    saved_tokens: AtomicUsize,
    total_tokens_processed: AtomicUsize,
}

impl TokenMetrics {
    pub fn new() -> Self {
        Self {
            saved_tokens: AtomicUsize::new(0),
            total_tokens_processed: AtomicUsize::new(0),
        }
    }

    /// Very rough heuristic: 1 token ~ 4 characters
    pub fn record_optimization(&self, original_len: usize, compressed_len: usize) {
        if original_len > compressed_len {
            let saved_chars = original_len - compressed_len;
            let saved_toks = saved_chars / 4;
            self.saved_tokens.fetch_add(saved_toks, Ordering::Relaxed);
            self.total_tokens_processed
                .fetch_add(original_len / 4, Ordering::Relaxed);
        } else {
            self.total_tokens_processed
                .fetch_add(original_len / 4, Ordering::Relaxed);
        }
    }

    pub fn get_saved_tokens(&self) -> usize {
        self.saved_tokens.load(Ordering::Relaxed)
    }

    pub fn get_total_processed(&self) -> usize {
        self.total_tokens_processed.load(Ordering::Relaxed)
    }
}

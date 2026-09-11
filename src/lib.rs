//! GGUF Janitor core library.
//!
//! Scan local LLM model stores (Ollama, LM Studio, HuggingFace cache, loose
//! folders), parse GGUF headers, find content-identical duplicates and reclaim
//! space via NTFS hardlinks, the Recycle Bin, or verified archive moves.

pub mod actions;
pub mod dedupe;
pub mod discover;
pub mod fit;
pub mod gguf;
pub mod license;

use serde::Serialize;

/// Which model store a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    Ollama,
    LmStudio,
    HuggingFace,
    Custom,
    Loose,
}

impl StoreKind {
    pub fn label(&self) -> &'static str {
        match self {
            StoreKind::Ollama => "Ollama",
            StoreKind::LmStudio => "LM Studio",
            StoreKind::HuggingFace => "HuggingFace",
            StoreKind::Custom => "Custom",
            StoreKind::Loose => "Loose",
        }
    }
}

/// A single model file found on disk.
#[derive(Debug, Clone, Serialize)]
pub struct ModelFile {
    pub path: String,
    pub size: u64,
    pub store: StoreKind,
    /// Populated when the file has a GGUF header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gguf: Option<gguf::GgufSummary>,
    /// For multi-volume splits: grouping key shared by all shards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_group: Option<String>,
    /// Ollama model tag (e.g. "llama3:8b") when resolvable from the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_tag: Option<String>,
}

/// Aggregated info about one discovered store.
#[derive(Debug, Clone, Serialize)]
pub struct StoreSummary {
    pub kind: StoreKind,
    pub root: String,
    pub files: u64,
    pub bytes: u64,
    /// True when the store's default location was not found on this machine.
    pub missing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    Discovering,
    Parsing,
    Hashing,
    Done,
}

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub phase: ProgressPhase,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: String,
}

/// Total results of one scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanResult {
    pub stores: Vec<StoreSummary>,
    pub files: Vec<ModelFile>,
    pub total_files: u64,
    pub total_bytes: u64,
}

/// Format a byte count for display.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut u = 0usize;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_bytes_formats() {
        use super::human_bytes;
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.00 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.00 GB");
    }
}

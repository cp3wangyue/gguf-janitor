//! Rough memory-fit estimation for running a GGUF model locally.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceMemory {
    /// Total system RAM in bytes (0 when unknown).
    pub ram_total: u64,
    /// Total VRAM per GPU in bytes; empty when no GPU query succeeded.
    pub vram_per_gpu: Vec<u64>,
    /// How VRAM was probed.
    pub vram_source: &'static str,
}

/// Probe system RAM and GPU VRAM. Never fails: fields are simply 0/empty when
/// the source is unavailable.
pub fn probe_devices() -> DeviceMemory {
    let ram_total = sysinfo::System::new_all().total_memory();
    let (vram_per_gpu, vram_source) = probe_vram();
    DeviceMemory { ram_total, vram_per_gpu, vram_source }
}

fn probe_vram() -> (Vec<u64>, &'static str) {
    // nvidia-smi is the most reliable cross-vendor(=nvidia) source on Windows.
    if let Ok(out) = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout);
            let gpus: Vec<u64> = txt
                .lines()
                .filter_map(|l| l.trim().parse::<u64>().ok())
                .map(|mib| mib * 1024 * 1024)
                .collect();
            if !gpus.is_empty() {
                return (gpus, "nvidia-smi");
            }
        }
    }
    (Vec::new(), "unknown")
}

/// Inputs for the estimate; all byte values.
#[derive(Debug, Clone, Serialize)]
pub struct ModelFit {
    /// Sum of the model file sizes (all shards for splits).
    pub weights_bytes: u64,
    /// Estimated KV-cache bytes at `context_tokens`.
    pub kv_cache_bytes: u64,
    /// Flat allowance for activations, CUDA graphs, sampler state.
    pub overhead_bytes: u64,
    pub context_tokens: u64,
    pub total_estimate_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FitVerdict {
    pub fits_largest_gpu: Option<bool>,
    pub fits_all_ram: Option<bool>,
    /// Short human explanation.
    pub note: String,
}

/// Estimate what it takes to run the described model.
///
/// KV cache = K + V per layer: `2 * n_layers * n_embd_kv * ctx * cache_bytes`.
/// `n_embd_kv` = `n_embd * n_head_kv / n_head`. When attention metadata is
/// missing, fall back to a heuristic of `2 * n_layers * n_embd/4 * ctx * 2`.
pub fn estimate(
    weights_bytes: u64,
    n_layers: Option<u64>,
    n_embd: Option<u64>,
    n_head: Option<u64>,
    n_head_kv: Option<u64>,
    context_tokens: u64,
) -> ModelFit {
    let overhead_bytes: u64 = 1536 * 1024 * 1024; // 1.5 GiB
    let kv_cache_bytes = match (n_layers, n_embd, n_head) {
        (Some(layers), Some(embd), head) => {
            let n_head = head.unwrap_or(1).max(1);
            let n_head_kv = n_head_kv.unwrap_or(n_head).max(1);
            let head_dim = embd / n_head.max(1);
            let n_embd_kv = head_dim * n_head_kv;
            2u64
                .saturating_mul(layers)
                .saturating_mul(n_embd_kv.max(embd / 4))
                .saturating_mul(context_tokens)
                .saturating_mul(2) // f16 cache
        }
        // Conservative fallback: ~0.5 byte/param at 4k ctx.
        _ => weights_bytes / 8,
    };
    let total = weights_bytes
        .saturating_add(kv_cache_bytes)
        .saturating_add(overhead_bytes);
    ModelFit {
        weights_bytes,
        kv_cache_bytes,
        overhead_bytes,
        context_tokens: context_tokens,
        total_estimate_bytes: total,
    }
}

/// Compare an estimate against the machine's memory.
pub fn verdict(fit: &ModelFit, dev: &DeviceMemory) -> FitVerdict {
    let fits_largest_gpu = (!dev.vram_per_gpu.is_empty())
        .then(|| dev.vram_per_gpu.iter().max().copied().unwrap_or(0) >= fit.total_estimate_bytes);
    let fits_all_ram = (dev.ram_total > 0)
        .then(|| dev.ram_total >= fit.total_estimate_bytes);

    let note = match (fits_largest_gpu, fits_all_ram) {
        (Some(true), _) => "fits on the largest GPU".to_string(),
        (Some(false), Some(true)) => "GPU-bound: likely CPU offload needed".to_string(),
        (Some(false), Some(false)) => "does not fit in RAM as-is".to_string(),
        (None, Some(true)) => "fits in system RAM (no GPU detected)".to_string(),
        (None, Some(false)) => "does not fit in RAM (no GPU detected)".to_string(),
        (_, None) => "memory unknown: cannot judge".to_string(),
    };
    FitVerdict { fits_largest_gpu, fits_all_ram, note }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(ram_gb: u64, vram_gb: Vec<u64>) -> DeviceMemory {
        DeviceMemory {
            ram_total: ram_gb * 1024 * 1024 * 1024,
            vram_per_gpu: vram_gb.into_iter().map(|g| g * 1024 * 1024 * 1024).collect(),
            vram_source: "test",
        }
    }

    /// Llama-3-8B-ish: 32 layers, 4096 embd, 32 heads / 8 KV heads, 8k ctx.
    const L8B: (u64, u64, u64, u64, u64, u64) = (32, 4096, 32, 8, 8192, 0);

    #[test]
    fn kv_cache_formula_matches_reference() {
        // reference: 2 * 32 layers * (128 head_dim * 8 kv_heads) * 8192 ctx * 2 bytes
        let (layers, embd, head, head_kv, ctx, weights) = L8B;
        let fit = estimate(weights, Some(layers), Some(embd), Some(head), Some(head_kv), ctx);
        let _head_dim = 4096 / 32; // 128
        let expect_kv = 2 * 32 * (128 * 8) * 8192 * 2;
        assert_eq!(fit.kv_cache_bytes, expect_kv);
        assert_eq!(fit.total_estimate_bytes, weights + expect_kv + 1536 * 1024 * 1024);
    }

    #[test]
    fn missing_attention_metadata_falls_back() {
        let fit = estimate(4_000_000_000, None, None, None, None, 4096);
        assert_eq!(fit.kv_cache_bytes, 500_000_000);
    }

    #[test]
    fn gqa_small_kv_heads_shrink_cache() {
        // Full MHA (kv heads = heads) should be 4x bigger than 8/32 GQA.
        let (layers, embd, head, _head_kv, ctx, weights) = L8B;
        let full = estimate(weights, Some(layers), Some(embd), Some(head), Some(head), ctx);
        let gqa = estimate(weights, Some(layers), Some(embd), Some(head), Some(8), ctx);
        assert_eq!(full.kv_cache_bytes, gqa.kv_cache_bytes * 4);
    }

    #[test]
    fn verdicts() {
        let (layers, embd, head, head_kv, ctx, weights) = L8B;
        let weights = 4_700_000_000u64; // Q4_K_M 8B
        let fit = estimate(weights, Some(layers), Some(embd), Some(head), Some(head_kv), ctx);
        // ~4.7GB + 1GB kv + 1.5GB overhead ≈ 7.2GB
        assert!(fit.total_estimate_bytes > 7_000_000_000);
        assert!(fit.total_estimate_bytes < 7_500_000_000);

        assert_eq!(verdict(&fit, &dev(16, vec![6])).note, "GPU-bound: likely CPU offload needed");
        assert_eq!(verdict(&fit, &dev(16, vec![12])).note, "fits on the largest GPU");
        assert_eq!(verdict(&fit, &dev(4, vec![])).note, "does not fit in RAM (no GPU detected)");
        assert_eq!(verdict(&fit, &dev(0, vec![])).note, "memory unknown: cannot judge");
    }
}

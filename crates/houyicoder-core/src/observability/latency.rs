//! Bucketed latency estimation: fixed-size histograms that answer p50/p99
//! without retaining every sample.
//!
//! Recording is O(1) and percentile lookup walks at most sixteen buckets, so a
//! long session's tail latency costs no more to report than a short one's.

use serde::{Deserialize, Serialize};

/// Bucketed latency histogram for p50/p99 estimation without retaining
/// every sample. Sixteen log2-spaced buckets span 1 ms to ~33 s; the final
/// bucket catches everything beyond. Recording is O(1); percentile lookup
/// walks at most 16 buckets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyHistogram {
    buckets: [u32; 16],
    total_samples: u32,
}

/// Upper-bound duration (ms) for each of the 16 buckets. Bucket i holds
/// samples in (BOUNDS[i-1], BOUNDS[i]]; bucket 0 holds [0, 1]. The last
/// bucket holds everything above BOUNDS[14].
const HISTOGRAM_BOUNDS_MS: [u64; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
];

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: [0; 16],
            total_samples: 0,
        }
    }

    /// Increment the bucket that covers the given duration. O(1).
    pub fn record(&mut self, duration_ms: u64) {
        let idx = HISTOGRAM_BOUNDS_MS
            .iter()
            .position(|&b| duration_ms <= b)
            .unwrap_or(HISTOGRAM_BOUNDS_MS.len());
        let idx = idx.min(self.buckets.len() - 1);
        self.buckets[idx] += 1;
        self.total_samples += 1;
    }

    /// Estimate the p-th percentile (0.0..=1.0) duration in ms. Walks the
    /// buckets accumulating counts until the target percentile is reached,
    /// then returns the bucket upper bound. Returns 0 when empty.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.total_samples == 0 {
            return 0;
        }
        let target = (self.total_samples as f64 * p).ceil() as u32;
        let mut running = 0u32;
        for (i, &count) in self.buckets.iter().enumerate() {
            running += count;
            if running >= target {
                return HISTOGRAM_BOUNDS_MS[i.min(HISTOGRAM_BOUNDS_MS.len() - 1)];
            }
        }
        HISTOGRAM_BOUNDS_MS[HISTOGRAM_BOUNDS_MS.len() - 1]
    }

    pub fn total_samples(&self) -> u32 {
        self.total_samples
    }
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

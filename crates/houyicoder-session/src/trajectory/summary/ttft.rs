//! The time-to-first-token distribution the summary reports.
//!
//! A session can run for hours, so a distribution that keeps every sample
//! grows without bound while the figures drawn from it stay a handful of
//! numbers. The samples live in fixed buckets whose width grows with the
//! value they hold, which bounds both what is stored and what a percentile
//! walk costs, at the price of reporting a percentile only to the width of
//! its bucket.

/// Buckets for the fine range, 50 ms each up to 10 s.
const TTFT_FINE_BUCKETS: usize = 200;
/// Buckets for the coarse range, 250 ms each up to 60 s.
const TTFT_COARSE_BUCKETS: usize = 200;
/// Buckets for the wide range, 5 s each up to 600 s.
const TTFT_WIDE_BUCKETS: usize = 108;
/// Buckets in total: the three ranges plus one for anything beyond them.
const TTFT_BUCKETS: usize = TTFT_FINE_BUCKETS + TTFT_COARSE_BUCKETS + TTFT_WIDE_BUCKETS + 1;

/// The width of the fine range in ms.
const TTFT_FINE_MS: u64 = 50;
/// Where the coarse range starts, in ms.
const TTFT_COARSE_START_MS: u64 = TTFT_FINE_MS * TTFT_FINE_BUCKETS as u64;
/// The width of the coarse range in ms.
const TTFT_COARSE_MS: u64 = 250;
/// Where the wide range starts, in ms.
const TTFT_WIDE_START_MS: u64 = TTFT_COARSE_START_MS + TTFT_COARSE_MS * TTFT_COARSE_BUCKETS as u64;
/// The width of the wide range in ms.
const TTFT_WIDE_MS: u64 = 5_000;

/// A time-to-first-token distribution held in fixed buckets.
///
/// Recording is constant time and a percentile walks a fixed number of
/// buckets, so neither grows with the session. The price is precision: a
/// percentile is the upper bound of the bucket that holds the rank, so it is
/// reported to the width of that bucket. The average stays exact.
pub(super) struct TtftHistogram {
    buckets: [u32; TTFT_BUCKETS],
    pub(super) samples: u64,
    pub(super) sum_ms: u64,
}

impl Default for TtftHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; TTFT_BUCKETS],
            samples: 0,
            sum_ms: 0,
        }
    }
}

impl TtftHistogram {
    pub(super) fn record(&mut self, ms: u64) {
        let index = bucket_of(ms);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.sum_ms = self.sum_ms.saturating_add(ms);
    }

    /// The upper bound of the bucket holding the given rank, and whether that
    /// bucket is the overflow one, whose true value is at least the bound.
    pub(super) fn percentile(&self, p: f64) -> Option<(u64, bool)> {
        if self.samples == 0 {
            return None;
        }
        let target = ((self.samples as f64 * p).ceil() as u64).max(1);
        let mut seen: u64 = 0;
        for (i, count) in self.buckets.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= target {
                let overflow = i == TTFT_BUCKETS - 1;
                return Some((bucket_upper_bound(i), overflow));
            }
        }
        Some((bucket_upper_bound(TTFT_BUCKETS - 1), true))
    }
}

/// The bucket a latency falls in.
fn bucket_of(ms: u64) -> usize {
    if ms < TTFT_COARSE_START_MS {
        (ms / TTFT_FINE_MS) as usize
    } else if ms < TTFT_WIDE_START_MS {
        TTFT_FINE_BUCKETS + ((ms - TTFT_COARSE_START_MS) / TTFT_COARSE_MS) as usize
    } else if ms < TTFT_WIDE_START_MS + TTFT_WIDE_MS * TTFT_WIDE_BUCKETS as u64 {
        TTFT_FINE_BUCKETS
            + TTFT_COARSE_BUCKETS
            + ((ms - TTFT_WIDE_START_MS) / TTFT_WIDE_MS) as usize
    } else {
        TTFT_BUCKETS - 1
    }
}

/// The largest latency the bucket can hold.
fn bucket_upper_bound(index: usize) -> u64 {
    if index < TTFT_FINE_BUCKETS {
        (index as u64 + 1) * TTFT_FINE_MS
    } else if index < TTFT_FINE_BUCKETS + TTFT_COARSE_BUCKETS {
        TTFT_COARSE_START_MS + (index - TTFT_FINE_BUCKETS + 1) as u64 * TTFT_COARSE_MS
    } else if index < TTFT_BUCKETS - 1 {
        TTFT_WIDE_START_MS
            + (index - TTFT_FINE_BUCKETS - TTFT_COARSE_BUCKETS + 1) as u64 * TTFT_WIDE_MS
    } else {
        TTFT_WIDE_START_MS + TTFT_WIDE_MS * TTFT_WIDE_BUCKETS as u64
    }
}

#[cfg(test)]
mod tests;

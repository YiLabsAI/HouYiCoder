//! Tests for the bucket scale and the percentiles read from it.

use super::{TTFT_BUCKETS, TtftHistogram, bucket_of, bucket_upper_bound};

/// A latency lands in the range its width covers, and the bucket reports the
/// upper bound of that range rather than the latency itself.
#[test]
fn test_bucket_rows_span_ranges() {
    let rows = [
        (0, 0, 50),
        (9_999, 199, 10_000),
        (10_000, 200, 10_250),
        (59_999, 399, 60_000),
        (60_000, 400, 65_000),
        (599_999, 507, 600_000),
    ];
    for (ms, index, upper) in rows {
        assert_eq!(bucket_of(ms), index, "{ms} ms");
        assert_eq!(bucket_upper_bound(index), upper, "{ms} ms");
    }
}

/// A latency past the widest range shares the last bucket, whose bound is the
/// widest range's end rather than a made-up value.
#[test]
fn test_overflow_bucket_caps() {
    let last = TTFT_BUCKETS - 1;
    assert_eq!(bucket_of(600_000), last);
    assert_eq!(bucket_of(u64::MAX), last);
    assert_eq!(bucket_upper_bound(last), 600_000);
}

/// A percentile reads the bucket holding the rank, so a sample in the coarse
/// range reports that bucket's bound and no overflow flag.
#[test]
fn test_percentile_reads_coarse() {
    let mut histogram = TtftHistogram::default();
    for ms in [12_000, 13_000, 14_000, 15_000] {
        histogram.record(ms);
    }
    assert_eq!(histogram.samples, 4);
    assert_eq!(histogram.sum_ms, 54_000);
    assert_eq!(histogram.percentile(0.95), Some((15_250, false)));
}

/// A histogram with no sample answers no percentile rather than zero.
#[test]
fn test_percentile_empty() {
    let histogram = TtftHistogram::default();
    assert_eq!(histogram.percentile(0.95), None);
}

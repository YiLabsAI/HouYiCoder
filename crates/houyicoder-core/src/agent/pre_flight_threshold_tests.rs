use super::pre_flight_threshold;

#[test]
fn test_buffer_reserves_output_room() {
    // 200k window, 8k output: reserve 21k, threshold 179k. A 95% ratio
    // would put the threshold at 190k — too thin for an 8-16k response.
    assert_eq!(pre_flight_threshold(200_000, 8_000), 179_000);
}

#[test]
fn test_huge_max_output_capped() {
    // A 50k max_output is capped at 20k so it cannot erase the buffer;
    // reserve 33k, threshold 167k.
    assert_eq!(pre_flight_threshold(200_000, 50_000), 167_000);
}

#[test]
fn test_scales_to_million_window() {
    // 1M window, 8k output: reserve 21k, threshold 979k (no 200k miscompute;
    // the ratio would put it at 950k, erasing 29k of usable headroom).
    assert_eq!(pre_flight_threshold(1_000_000, 8_000), 979_000);
}

#[test]
fn test_tiny_window_saturates() {
    // A 200-token window smaller than the reserve: threshold 0, compress
    // trips on any non-empty served view.
    assert_eq!(pre_flight_threshold(200, 8_000), 0);
}

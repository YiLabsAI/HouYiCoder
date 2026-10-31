//! A byte count in the short decimal form a pane prints: 512 B, 22.5 KiB,
//! 1.4 MiB. Shared so every view that names a stored size names it the same
//! way.

/// A byte count as a short decimal size with a binary unit. Sizes below a
/// kibibyte stay whole bytes, because that is the scale where a fraction says
/// nothing.
pub fn format_byte_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    loop {
        // Round before choosing the unit. A size a hair under the boundary
        // prints as 1024.0 in the smaller unit, which reads as a number the
        // unit cannot hold; the rounded value promotes instead.
        let rounded = (value * 10.0).round() / 10.0;
        if rounded < 1024.0 || unit + 1 >= UNITS.len() {
            return format!("{rounded:.1} {}", UNITS[unit]);
        }
        value /= 1024.0;
        unit += 1;
    }
}

#[cfg(test)]
mod tests;

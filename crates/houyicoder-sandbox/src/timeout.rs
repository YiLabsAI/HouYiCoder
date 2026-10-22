//! The wall-clock message every backend reports when a command overruns.

/// Report the budget in seconds. A whole-second division turned a 200ms budget
/// into "0s", which reads as a measured zero rather than a small limit, so the
/// value keeps its fraction and drops it only when it is whole.
pub(crate) fn message(millis: u64) -> String {
    let seconds = millis as f64 / 1000.0;
    format!("wall-clock {seconds}s exceeded")
}

#[cfg(test)]
mod tests;

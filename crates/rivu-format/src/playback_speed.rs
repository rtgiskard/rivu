//! Playback multiplier limits and keyboard adjustment shared by playback hosts.

pub const MIN: f64 = 0.5;
pub const MAX: f64 = 2.0;
pub const STEP: f64 = 0.05;

/// Round adjustments to hundredths before clamping to the supported range.
pub fn adjust(speed: f64, delta: f64) -> f64 {
    (((speed + delta) * 100.0).round() / 100.0).clamp(MIN, MAX)
}

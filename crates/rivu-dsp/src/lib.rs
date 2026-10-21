//! Audio transformations own their numerical state, not devices or application state.
//! Prepare configurations outside the output callback; process bounded PCM blocks.
pub mod denoise;
pub mod equalizer;
pub mod fir_eq;

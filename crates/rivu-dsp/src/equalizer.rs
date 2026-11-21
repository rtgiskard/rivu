//! Shared parametric EQ contracts and alternative numerical implementations.
pub mod config;
#[cfg(feature = "execution")]
pub mod fir;
#[cfg(feature = "execution")]
pub mod iir;

#[cfg(all(test, feature = "execution"))]
const FILTER_KINDS: [config::FilterKind; 7] = {
    use config::FilterKind::*;
    [
        Peak, LowShelf, HighShelf, LowPass, HighPass, BandPass, Notch,
    ]
};

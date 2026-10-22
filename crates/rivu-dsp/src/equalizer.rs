//! Shared parametric EQ contracts and alternative numerical implementations.
pub mod config;
#[cfg(feature = "execution")]
pub mod fir;
#[cfg(feature = "execution")]
pub mod iir;

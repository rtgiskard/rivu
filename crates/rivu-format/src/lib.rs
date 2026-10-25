//! Current persisted format contracts, independent of the application runtime.
pub mod config;
#[cfg(feature = "database")]
pub mod database;
pub mod input;
pub mod ownership;

//! Application configuration persistence and logging integration.

pub use rivu_format::config::*;

mod persistence;
pub use persistence::ConfigPersistence;

#[cfg(test)]
mod tests;

pub(crate) fn logging_options(config: &Config) -> rivu_utils::logging::Options {
    use rivu_utils::logging;

    logging::Options {
        level: match config.log_level {
            LogLevel::Debug => logging::LogLevel::Debug,
            LogLevel::Info => logging::LogLevel::Info,
            LogLevel::Warning => logging::LogLevel::Warning,
            LogLevel::Error => logging::LogLevel::Error,
        },
        to_file: config.log_to_file,
        max_log_files: config.log_max_files,
    }
}

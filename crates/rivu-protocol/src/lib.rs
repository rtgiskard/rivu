//! Shared IPC contracts and transport, independent of the application owner.

pub mod model;
pub mod response;
pub mod transport;
pub mod wire;

#[cfg(test)]
mod tests;

use model::Command;
use std::time::Duration;

pub const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(12);
const OPTIMIZE_REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Total caller waiting budget, including queue admission and the query reply.
pub fn query_wait_timeout() -> Duration {
    ADMISSION_TIMEOUT + REPLY_TIMEOUT
}

/// Reply budget after admission. Timing out never revokes admitted work.
pub fn command_reply_timeout(command: &Command) -> Duration {
    match command {
        Command::OptimizeDatabase => OPTIMIZE_REPLY_TIMEOUT,
        _ => REPLY_TIMEOUT,
    }
}

/// Application requests are never business commands or core view state.
#[derive(Clone, Debug)]
pub enum AppCommand {
    Core(Command),
    ShowWindow,
    Shutdown,
}

impl AppCommand {
    /// Total operation wait, before IPC bridge and response transport allowances.
    pub fn wait_timeout(&self) -> Duration {
        match self {
            Self::Core(command) => ADMISSION_TIMEOUT + command_reply_timeout(command),
            Self::ShowWindow | Self::Shutdown => REPLY_TIMEOUT,
        }
    }
}

impl From<Command> for AppCommand {
    fn from(command: Command) -> Self {
        Self::Core(command)
    }
}

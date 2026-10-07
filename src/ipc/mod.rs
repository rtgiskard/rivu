mod client;
mod protocol;
mod server;
mod transport;

use protocol::*;
use transport::*;

pub(crate) use client::query_with_cancel;
pub use client::request_ack;
pub use client::{WatcherSession, get_state, query, watch_session, watch_session_with_cancel};
pub use server::{Server, bind, is_no_instance};

#[cfg(test)]
mod tests;

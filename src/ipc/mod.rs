mod client;
pub(crate) mod endpoint;
mod protocol;
mod server;
mod transport;

use protocol::*;
use transport::*;

pub use client::{
    ClientSession, InstanceChanged, client_session, client_session_with_cancel, get_state, query,
    request_ack,
};
pub use server::{Server, SocketOwner, is_no_instance};

#[cfg(test)]
pub(crate) mod tests;

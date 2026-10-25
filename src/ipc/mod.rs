mod server;
pub use server::{Server, SocketOwner, is_no_instance};

#[cfg(test)]
pub(crate) mod tests;

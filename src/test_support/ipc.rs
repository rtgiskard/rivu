pub use rivu_utils::test_support::SocketTestDirectory;

pub fn socket_test_directory() -> SocketTestDirectory {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    SocketTestDirectory::new_in(&cache)
}

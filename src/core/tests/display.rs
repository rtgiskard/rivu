use super::*;

#[test]
fn render_snapshot_rejects_a_source_retired_before_owner_dispatch() {
    let (_directory, mut core) = fixture();
    core.state.source_generation = 2;
    let (reply, receiver) = oneshot::channel();
    core.dispatch_request(Request {
        operation: Operation::RenderSnapshot { source: 1, reply },
        reply: None,
    });
    let result = futures::executor::block_on(receiver).unwrap();
    assert!(result.is_err());
}

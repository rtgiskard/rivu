use super::*;

#[test]
fn waveform_snapshot_rejects_a_source_retired_before_owner_dispatch() {
    let (_directory, mut core) = fixture();
    core.state.source_generation = 2;
    let (reply, receiver) = oneshot::channel();
    core.dispatch_request(Request {
        operation: Operation::WaveformSnapshot { source: 1, reply },
        reply: None,
    });
    let result = futures::executor::block_on(receiver).unwrap();
    assert_eq!(
        result.unwrap_err(),
        "Full waveform source changed before capture"
    );
}

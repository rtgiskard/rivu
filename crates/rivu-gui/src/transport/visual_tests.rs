use super::*;
use rivu_analysis::VisualizerDemand;
use rivu_protocol::pcm::{PcmHeader, REQUEST_SIZE, write_status};

struct Fixture {
    window: SharedPcmWindow,
    client: VisualClient,
    server: AsyncStream,
    wakes: Receiver<()>,
}

fn fixture() -> Fixture {
    let window = SharedPcmWindow::new().unwrap();
    let reader = SharedPcmWindow::open(&window.share_path()).unwrap();
    let (socket, server) = UnixStream::pair().unwrap();
    socket.set_nonblocking(true).unwrap();
    server.set_nonblocking(true).unwrap();
    let (notifications, receiver) = async_channel::bounded(1);
    let (wake, wakes) = async_channel::bounded(1);
    let state = Arc::new(ClientState {
        desired: Mutex::new(Desired {
            settings: AnalysisSettings {
                fps: 60,
                fft_size: 512,
                ..AnalysisSettings::from(&Config::default())
            },
            ..Desired::default()
        }),
        current: RwLock::new(None),
        wakeup: RwLock::new(Some(Arc::new(move || {
            let _ = wake.try_send(());
        }))),
        error: Mutex::new(ErrorState::default()),
        stopped: AtomicBool::new(false),
    });
    let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
    let worker_state = state.clone();
    let staging = analysis.clone();
    let stream = Async::new(socket.try_clone().unwrap()).unwrap();
    let worker = thread::spawn(move || {
        if let Err(error) = async_io::block_on(run_client(
            stream,
            reader,
            &staging,
            &worker_state,
            receiver,
        )) && !worker_state.stopped.load(Ordering::Acquire)
        {
            worker_state.fail(error.to_string());
        }
    });
    Fixture {
        window,
        client: VisualClient {
            analysis,
            state,
            notifications,
            socket,
            worker: Mutex::new(Some(worker)),
        },
        server: Async::new(server).unwrap(),
        wakes,
    }
}

fn header(state: PcmState, start_frame: u64) -> PcmHeader {
    PcmHeader {
        epoch: state.epoch,
        intent: state.intent,
        generation: state.generation,
        capture: state.capture,
        start_frame,
        output_frame: start_frame,
        sample_rate: state.sample_rate,
        ..PcmHeader::default()
    }
}

fn stereo(window: &SharedPcmWindow, header: PcmHeader, value: f32, frames: usize) {
    assert!(window.publish(header, &[0, 1], (0..frames * 2).map(|_| value)));
}

async fn current_after(f: &Fixture, sequence: u64) -> AnalysisFrame {
    rivu_protocol::transport::timeout(Duration::from_secs(5), async {
        loop {
            let mut accepted = None;
            f.client.consume_current_analysis(|frame| {
                if frame.sequence > sequence {
                    accepted = Some(frame.clone());
                    true
                } else {
                    false
                }
            });
            if let Some(frame) = accepted {
                return frame;
            }
            f.wakes.recv().await.unwrap();
        }
    })
    .await
    .expect("shared PCM analysis did not advance")
}

async fn subscribe(f: &mut Fixture) {
    f.client.set_display_demand(DisplayDemand {
        enabled: true,
        spectrum: true,
        visualizer: VisualizerDemand {
            pcm: true,
            ..VisualizerDemand::default()
        },
    });
    ControlReader::<REQUEST_SIZE>::new()
        .read(&mut f.server)
        .await
        .unwrap();
    // Wait until the local capture has retired the initial shared prefix.
    write_status(
        &mut f.server,
        PcmState {
            epoch: 2,
            ..PcmState::default()
        },
        1,
        0,
    )
    .await
    .unwrap();
    rivu_protocol::transport::timeout(Duration::from_secs(5), async {
        while f.client.analysis.read().audio_epoch != 2 {
            f.wakes.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[test]
fn analysis_copy_holds_acceptance_guards_and_revocation_never_calls_consumer() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let state = PcmState {
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..PcmState::default()
        };
        stereo(&f.window, header(state, 0), 0.25, 512);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let accepted = current_after(&f, 0).await;
        let mut staging_sequence = 0;
        assert!(f.client.consume_current_analysis(|frame| {
            assert!(f.client.analysis.try_write().is_none());
            assert!(f.client.state.desired.try_lock().is_none());
            assert!(f.client.state.current.try_write().is_none());
            staging_sequence = frame.sequence;
            true
        }));
        assert_eq!(staging_sequence, accepted.sequence);
        f.client.invalidate();
        let mut called = false;
        assert!(!f.client.consume_current_analysis(|frame| {
            called = true;
            staging_sequence = frame.sequence;
            true
        }));
        assert!(!called);
        assert_eq!(staging_sequence, accepted.sequence);
    });
}

#[test]
fn fresh_pcm_publishes_without_gui_permits_and_resumes_after_visibility_changes() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let state = PcmState {
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..PcmState::default()
        };
        let config = Config {
            analysis_fft_size: 512,
            ..Config::default()
        };
        f.client.configure(&config);
        stereo(&f.window, header(state, 0), 0.25, 512);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let first = current_after(&f, 0).await;
        stereo(&f.window, header(state, 512), 0.5, 512);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let second = current_after(&f, first.sequence).await;
        assert_eq!(second.rms_left, 0.5);

        let demand = f.client.state.desired.lock().demand;
        f.client.set_display_demand(DisplayDemand::default());
        ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        write_status(&mut f.server, state, 2, 0).await.unwrap();
        assert!(!f.client.analysis_frame_is_current(&second));
        stereo(&f.window, header(state, 1024), 0.875, 512);
        f.client.set_display_demand(demand);
        ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        let token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().display_token != token {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let minimum = f.window.sequence();
        write_status(&mut f.server, state, 3, minimum)
            .await
            .unwrap();
        stereo(&f.window, header(state, 1536), 0.125, 512);
        write_status(&mut f.server, state, 3, minimum)
            .await
            .unwrap();
        let restored = current_after(&f, second.sequence).await;
        assert_eq!(restored.rms_left, 0.125);
        assert_eq!(restored.display_token, token);
    });
}

#[test]
fn shared_native_channels_follow_cursor_and_reject_retired_capture() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let state = PcmState {
            epoch: 4,
            active: true,
            sample_rate: 48_000,
            ..PcmState::default()
        };
        // Native layout is intentionally permuted: R, center, L. The GUI,
        // not Core, folds it into normalized logical stereo.
        let mut future = header(state, 0);
        future.output_frame = 10_000;
        assert!(f.window.publish(
            future,
            &[1, 2, 0],
            (0..512 * 3).map(|i| [0.25, 0.5, 1.0][i % 3])
        ));
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().audio_epoch != state.epoch {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(f.client.analysis.read().settings.is_none());
        // No earlier successful publication can hide premature ingestion behind
        // the display cadence deadline.
        // Cursor-only progress must release already-published samples without
        // another PCM publication or a timer polling the output clock.
        let advanced = PcmState {
            heard_frame: 10_000,
            ..state
        };
        write_status(&mut f.server, advanced, 1, 0).await.unwrap();
        let native = current_after(&f, 0).await;
        assert_eq!(native.rms_left, 0.75);
        assert_eq!(native.rms_right, 0.375);

        // A local revision fences existing history even if global capture did
        // not stop. A retired identity must never enter the fresh FFT window.
        f.client.invalidate();
        ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        // Sending the request precedes local retirement. Observe the revoked
        // staging publication before putting replacement PCM in the window.
        let display_token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().display_token != display_token {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let minimum = f.window.sequence();
        let next = PcmState {
            epoch: 6,
            intent: 1,
            applied_intent: 1,
            capture: 1,
            ..advanced
        };
        stereo(&f.window, header(state, 1024), 0.9, 512);
        stereo(&f.window, header(next, 0), 0.125, 512);
        write_status(&mut f.server, next, 2, minimum).await.unwrap();
        let fenced = current_after(&f, native.sequence).await;
        assert_eq!(fenced.rms_left, 0.125);
        assert_eq!(fenced.audio_epoch, next.epoch);
        assert!(!f.client.analysis_frame_is_current(&native));
    });
}

#[test]
fn subscription_frontier_excludes_pre_subscription_pcm_with_current_identity() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let state = PcmState {
            epoch: 4,
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..PcmState::default()
        };
        // Local retirement is complete, but the server's newer frontier must
        // still exclude this otherwise-valid prefix from the first FFT window.
        stereo(&f.window, header(state, 0), 0.9, 512);
        let minimum = f.window.sequence();
        stereo(&f.window, header(state, 512), 0.25, 256);
        write_status(&mut f.server, state, 1, minimum)
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().audio_epoch != state.epoch {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(f.client.analysis.read().settings.is_none());
        stereo(&f.window, header(state, 768), 0.25, 256);
        write_status(&mut f.server, state, 1, minimum)
            .await
            .unwrap();
        let fresh = current_after(&f, 0).await;
        assert_eq!(fresh.rms_left, 0.25);
        assert!(
            fresh
                .visualizer
                .pcm
                .iter()
                .all(|sample| *sample == [0.25; 2])
        );
    });
}

#[test]
fn overwritten_history_and_explicit_gap_require_a_fresh_window() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let state = PcmState {
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..PcmState::default()
        };
        stereo(&f.window, header(state, 0), 0.5, 512);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let first = current_after(&f, 0).await;
        for index in 0..PCM_QUEUE_BLOCKS * 2 {
            stereo(
                &f.window,
                header(state, 512 + index as u64 * 512),
                0.75,
                512,
            );
        }
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let caught_up = current_after(&f, first.sequence).await;
        assert_eq!(caught_up.rms_left, 0.75);
        assert!(caught_up.generation > first.generation);
        assert!(
            caught_up.sample_time
                < first.sample_time + Duration::from_secs_f64(64.0 * 512.0 / 48_000.0)
        );
        let start = 512 + (PCM_QUEUE_BLOCKS * 2) as u64 * 512;
        let mut gap = header(state, start);
        gap.gap = true;
        stereo(&f.window, gap, 0.25, 256);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            loop {
                let frame = f.client.analysis.read().clone();
                if frame.generation > caught_up.generation && frame.settings.is_none() {
                    break;
                }
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(!f.client.analysis_frame_is_current(&caught_up));
        stereo(&f.window, header(state, start + 256), 0.25, 256);
        write_status(&mut f.server, state, 1, 0).await.unwrap();
        let fresh = current_after(&f, caught_up.sequence).await;
        assert_eq!(fresh.rms_left, 0.25);
        assert!(
            fresh
                .visualizer
                .pcm
                .iter()
                .all(|sample| *sample == [0.25; 2])
        );
    });
}

#[test]
fn analysis_control_rejects_a_different_core_instance_with_a_valid_shared_window() {
    let directory = tempfile::tempdir().unwrap();
    let window = SharedPcmWindow::new().unwrap();
    let path = window.share_path();
    let listener =
        std::os::unix::net::UnixListener::bind_addr(&address(directory.path()).unwrap()).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut hello = [0; REQUEST_SIZE];
        stream.read_exact(&mut hello).unwrap();
        let root = path.as_os_str().as_encoded_bytes();
        let mut welcome = Vec::from(*MAGIC);
        welcome.extend_from_slice(&43u32.to_le_bytes());
        welcome.extend_from_slice(&(root.len() as u32).to_le_bytes());
        welcome.extend_from_slice(root);
        stream.write_all(&welcome).unwrap();
    });
    assert!(VisualClient::connect(directory.path(), 42).is_err());
    server.join().unwrap();
}

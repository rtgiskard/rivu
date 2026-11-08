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
        summary: Mutex::new(WaveformSummary::default()),
        waveform: Arc::new(RwLock::new(WaveformFrame::default())),
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
            waveform: Arc::clone(&state.waveform),
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
        media_start: start_frame as f64 / f64::from(state.sample_rate.max(1)),
        media_step: f64::from(state.sample_rate.max(1)).recip(),
        sample_rate: state.sample_rate,
        ..PcmHeader::default()
    }
}

fn stereo(window: &SharedPcmWindow, header: PcmHeader, value: f32, frames: usize) {
    assert!(window.publish(header, &[0, 1], (0..frames * 2).map(|_| value)));
}

#[test]
fn checked_shared_observation_discards_overwritten_and_reconfigured_payloads() {
    for reconfigure in [false, true] {
        let window = SharedPcmWindow::new().unwrap();
        let reader = SharedPcmWindow::open(&window.share_path()).unwrap();
        let settings = AnalysisSettings {
            fft_size: 512,
            ..AnalysisSettings::from(&Config::default())
        };
        let demand = DisplayDemand {
            enabled: true,
            spectrum: true,
            visualizer: VisualizerDemand {
                pcm: true,
                stereo_density: true,
                ..Default::default()
            },
        };
        let state = PcmState {
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..Default::default()
        };
        let mut engine = AnalysisEngine::new(settings);
        let mut frame = AnalysisFrame::default();
        engine.configure(settings, demand, state, 1, &mut frame);
        let generation = frame.generation;
        stereo(&window, header(state, 0), 0.25, 512);
        let block = reader.block(1).unwrap();
        let samples = (0..block.len()).map(|index| {
            if index == 256 {
                if reconfigure {
                    window.retain_output(12_000, 48_000, 3).unwrap();
                }
                // Replace the exact borrowed sample region halfway through
                // direct observation, without timing or a mocked validity flag.
                for publication in 0..window.retained_blocks() {
                    let next = header(state, 4096 + publication as u64 * 1024);
                    assert!(window.publish(next, &[17, 0, 1], (0..512 * 3).map(|_| 0.875),));
                }
            }
            let stereo = [block.sample(index, 0), block.sample(index, 1)];
            assert_eq!(stereo, if index < 256 { [0.25; 2] } else { [0.875; 2] });
            stereo
        });
        assert!(!engine.ingest_checked(
            block.header(),
            samples,
            || block.still_current(),
            &mut frame,
        ));
        assert!(frame.generation > generation);
        assert_eq!(frame.sample_time, Duration::ZERO);
        assert!(frame.settings.is_none());
        assert!(frame.spectrum_db.is_empty());
        assert!(frame.visualizer.pcm.is_empty());
        assert!(frame.visualizer.density.is_empty());
        engine.request_frame();
        let now = Instant::now();
        assert!(!engine.publish(&mut frame, now));

        stereo(&window, header(state, 200_000), 0.5, 512);
        let clean = reader.block(reader.sequence()).unwrap();
        assert!(engine.ingest_checked(
            clean.header(),
            (0..clean.len()).map(|index| [clean.sample(index, 0), clean.sample(index, 1)]),
            || clean.still_current(),
            &mut frame,
        ));
        engine.request_frame();
        assert!(engine.publish(&mut frame, now));
        let mut reference = AnalysisEngine::new(settings);
        let mut expected = AnalysisFrame::default();
        reference.configure(settings, demand, state, 1, &mut expected);
        assert!(reference.ingest(clean.header(), [[0.5; 2]; 512].into_iter(), &mut expected));
        reference.request_frame();
        assert!(reference.publish(&mut expected, now));
        assert_eq!(frame.sample_time, expected.sample_time);
        assert_eq!(frame.rms_left, expected.rms_left);
        assert_eq!(frame.spectrum_db, expected.spectrum_db);
        assert_eq!(frame.visualizer.pcm, expected.visualizer.pcm);
        assert_eq!(frame.visualizer.density, expected.visualizer.density);
    }
}

#[test]
fn latest_new_identity_tail_survives_lagged_control_status() {
    async_io::block_on(async {
        let mut f = fixture();
        subscribe(&mut f).await;
        let old = PcmState {
            epoch: 4,
            active: true,
            sample_rate: 48_000,
            heard_frame: u64::MAX,
            ..Default::default()
        };
        write_status(&mut f.server, old, 1, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().audio_epoch != old.epoch {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let next = PcmState { epoch: 6, ..old };
        stereo(&f.window, header(next, 0), 0.25, 128);
        let frontier = f.window.frontier();

        // A display-token publication is an observable barrier proving that
        // ingestion ran against the old status after the new prefix appeared.
        let mut demand = f.client.state.desired.lock().demand;
        demand.visualizer.band_energy = true;
        f.client.set_display_demand(demand);
        let token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            loop {
                let mut observed = false;
                f.client.consume_current_analysis(|frame| {
                    observed = frame.display_token == token && frame.audio_epoch == old.epoch;
                    observed
                });
                if observed {
                    break;
                }
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();

        stereo(&f.window, header(next, 128), 0.75, 512);
        assert_eq!(f.window.frontier().sequence, frontier.sequence);
        assert_eq!(f.window.frontier().offset, 640);
        write_status(&mut f.server, next, 1, PcmPosition::default())
            .await
            .unwrap();
        let recovered = current_after(&f, 0).await;
        assert_eq!(recovered.audio_epoch, next.epoch);
        assert_eq!(
            recovered.sample_time,
            Duration::from_secs_f64(640.0 / 48_000.0)
        );
        assert_eq!(recovered.rms_left, 0.75);
        assert!(
            recovered
                .visualizer
                .pcm
                .iter()
                .all(|sample| *sample == [0.75; 2])
        );
    });
}

async fn current_after(f: &Fixture, sequence: u64) -> AnalysisFrame {
    rivu_protocol::transport::timeout(Duration::from_secs(5), async {
        loop {
            let mut accepted = None;
            f.client.consume_current_analysis(|frame| {
                if frame.sequence > sequence && frame.settings.is_some() {
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
        PcmPosition::default(),
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
fn growing_partial_block_wakes_analysis_without_advancing_its_logical_sequence() {
    let mut f = fixture();
    async_io::block_on(subscribe(&mut f));
    let state = PcmState {
        active: true,
        sample_rate: 48_000,
        heard_frame: u64::MAX,
        ..PcmState::default()
    };
    stereo(&f.window, header(state, 0), 0.25, 256);
    let sequence = f.window.sequence();
    async_io::block_on(async {
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            loop {
                let mut accepted = false;
                f.client.consume_current_analysis(|frame| {
                    accepted = frame.audio_epoch == state.epoch && frame.settings.is_none();
                    accepted
                });
                if accepted {
                    break;
                }
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
    });
    thread::scope(|scope| {
        let (started, writing) = std::sync::mpsc::sync_channel(0);
        let (release, resume) = std::sync::mpsc::sync_channel(0);
        let window = &f.window;
        let publisher = scope.spawn(move || {
            window.publish(
                header(state, 256),
                &[0, 1],
                (0..512).map(|sample| {
                    if sample == 0 {
                        started.send(()).unwrap();
                        resume.recv().unwrap();
                    }
                    0.75
                }),
            )
        });
        writing.recv_timeout(Duration::from_secs(5)).unwrap();
        // Force an ingest turn while the current tail is write-fenced. It must
        // retain both this sequence and the already-consumed 256-frame prefix.
        let mut demand = f.client.state.desired.lock().demand;
        demand.visualizer.band_energy = true;
        f.client.set_display_demand(demand);
        let token = f.client.state.desired.lock().display_token;
        async_io::block_on(async {
            rivu_protocol::transport::timeout(Duration::from_secs(5), async {
                while f.client.analysis.read().display_token != token {
                    f.wakes.recv().await.unwrap();
                }
            })
            .await
            .unwrap();
        });
        release.send(()).unwrap();
        assert!(publisher.join().unwrap());
    });
    assert_eq!(f.window.sequence(), sequence);
    async_io::block_on(async {
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        let frame = current_after(&f, 0).await;
        assert!((frame.rms_left - 0.3125_f32.sqrt()).abs() < 1e-6);
        assert_eq!(&frame.visualizer.pcm[..256], &[[0.25; 2]; 256]);
        assert_eq!(&frame.visualizer.pcm[256..], &[[0.75; 2]; 256]);
    });
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
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
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
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        let first = current_after(&f, 0).await;
        stereo(&f.window, header(state, 512), 0.5, 512);
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        let second = current_after(&f, first.sequence).await;
        assert_eq!(second.rms_left, 0.5);

        let demand = f.client.state.desired.lock().demand;
        f.client.set_display_demand(DisplayDemand::default());
        ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        write_status(&mut f.server, state, 2, PcmPosition::default())
            .await
            .unwrap();
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
        let minimum = f.window.frontier();
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
        write_status(&mut f.server, state, 1, PcmPosition::default())
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
        // No earlier successful publication can hide premature ingestion behind
        // the display cadence deadline.
        // Cursor-only progress must release already-published samples without
        // another PCM publication or a timer polling the output clock.
        let advanced = PcmState {
            heard_frame: 10_000,
            ..state
        };
        write_status(&mut f.server, advanced, 1, PcmPosition::default())
            .await
            .unwrap();
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
        let minimum = f.window.frontier();
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
        let minimum = f.window.frontier();
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

pub(crate) fn with_gap_invalidation(
    consume: impl FnOnce(&VisualClient, &AnalysisFrame, &AnalysisFrame),
) {
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
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        let first = current_after(&f, 0).await;
        for index in 0..PCM_QUEUE_BLOCKS * 2 {
            stereo(
                &f.window,
                header(state, 512 + index as u64 * 512),
                0.75,
                512,
            );
        }
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
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
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            loop {
                let frame = f.client.analysis.read().clone();
                if frame.generation > caught_up.generation && frame.settings.is_none() {
                    assert!(f.client.analysis_frame_is_current(&frame));
                    assert!(frame.visualizer.pcm.is_empty());
                    assert!(frame.spectrum_db.is_empty());
                    consume(&f.client, &caught_up, &frame);
                    break;
                }
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(!f.client.analysis_frame_is_current(&caught_up));
        stereo(&f.window, header(state, start + 256), 0.25, 256);
        write_status(&mut f.server, state, 1, PcmPosition::default())
            .await
            .unwrap();
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
    let directory = rivu::test_support::ipc::socket_test_directory();
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
        let mut welcome = Vec::from(
            Welcome {
                instance: 43,
                path_length: root.len(),
            }
            .encode()
            .unwrap(),
        );
        welcome.extend_from_slice(root);
        stream.write_all(&welcome).unwrap();
    });
    assert!(VisualClient::connect(directory.path(), 42).is_err());
    server.join().unwrap();
}

async fn subscribe_waveform(f: &mut Fixture, track: &Track, generation: u64) -> (PcmState, u64) {
    f.client
        .configure_source(Some(track), generation, track.duration);
    let mut control = ControlReader::<REQUEST_SIZE>::new();
    let configured = control.read(&mut f.server).await.unwrap();
    assert_eq!(configured[1], 0, "source selection alone requests no PCM");
    f.client.set_waveform_demand(true);
    let request = control.read(&mut f.server).await.unwrap();
    assert_eq!(
        request[1], 1,
        "waveform-only enables the shared PCM producer"
    );
    let ControlRequest::Subscribe { revision, .. } = ControlRequest::decode(&request).unwrap()
    else {
        panic!("expected subscription");
    };
    let state = PcmState {
        epoch: 2,
        generation,
        active: true,
        sample_rate: track.sample_rate,
        heard_frame: u64::MAX,
        ..Default::default()
    };
    write_status(&mut f.server, state, revision, PcmPosition::default())
        .await
        .unwrap();
    rivu_protocol::transport::timeout(Duration::from_secs(5), async {
        while f.client.analysis.read().audio_epoch != state.epoch {
            f.wakes.recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    (state, revision)
}

async fn summary_after(f: &Fixture, revision: u64) -> WaveformFrame {
    rivu_protocol::transport::timeout(Duration::from_secs(5), async {
        loop {
            let ready = {
                let summary = f.client.state.summary.lock();
                (summary.frame.revision > revision).then(|| summary.frame.clone())
            };
            if let Some(frame) = ready {
                return frame;
            }
            async_io::Timer::after(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("hidden waveform summary did not advance")
}

#[test]
fn waveform_only_uses_shared_pcm_without_fft_and_growing_tails_are_unique() {
    async_io::block_on(async {
        let mut f = fixture();
        let mut track = crate::test_support::artwork_track("source.wav".into());
        track.sample_rate = 48_000;
        track.duration = Some(1.0);
        let (state, revision) = subscribe_waveform(&mut f, &track, 1).await;
        stereo(&f.window, header(state, 0), 0.5, 256);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.waveform.read().left_rms_counts.iter().sum::<u64>() != 256 {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        stereo(&f.window, header(state, 256), 0.25, 128);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.waveform.read().left_rms_counts.iter().sum::<u64>() != 384 {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        let analysis = f.client.analysis.read();
        assert!(analysis.settings.is_none());
        assert!(analysis.spectrum_db.is_empty());
        assert!(analysis.visualizer.pcm.is_empty());
        drop(analysis);
        let retained = f.client.waveform.read().peaks.clone();
        track.decoder_hint = rivu_format::decoder_hint::DecoderHint::Ffmpeg;
        track.title = "metadata only".into();
        f.client.configure_source(Some(&track), 1, track.duration);
        f.client.invalidate();
        assert_eq!(f.client.waveform.read().peaks, retained);
        f.client.configure_source(Some(&track), 2, track.duration);
        assert!(f.client.waveform.read().peaks.iter().all(Option::is_none));
    });
}

#[test]
fn visible_waveform_is_available_before_retained_backlog_finishes() {
    async_io::block_on(async {
        let mut f = fixture();
        let block_frames = rivu_protocol::pcm::PCM_BLOCK_FRAMES;
        let blocks = PCM_QUEUE_BLOCKS + 1;
        f.window
            .retain_output(blocks * block_frames, 48_000, 2)
            .unwrap();
        let mut track = crate::test_support::artwork_track("backlog.wav".into());
        track.sample_rate = 48_000;
        track.duration = Some(4.0);
        let (state, revision) = subscribe_waveform(&mut f, &track, 1).await;
        let initial = f.client.waveform.read().clone();
        let waveform = Arc::clone(&f.client.waveform);
        let (published, snapshots) = async_channel::unbounded();
        f.client.set_wakeup(Arc::new(move || {
            let _ = published.try_send(waveform.read().clone());
        }));
        for block in 0..blocks {
            stereo(
                &f.window,
                header(state, (block * block_frames) as u64),
                if block + 1 == blocks { 0.75 } else { 0.25 },
                block_frames,
            );
        }
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        let total = (blocks * block_frames) as u64;
        let (prefix, complete) = rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            let mut prefix = None;
            loop {
                let frame = snapshots.recv().await.unwrap();
                let count = frame.left_rms_counts.iter().sum::<u64>();
                if count == 0 {
                    continue;
                }
                if count == total {
                    break (
                        prefix.expect("backlog must expose a bounded-yield snapshot"),
                        frame,
                    );
                }
                if prefix.is_none() {
                    prefix = Some(frame);
                }
            }
        })
        .await
        .unwrap();
        assert!(prefix.left_rms_counts.iter().sum::<u64>() < total);
        assert!(prefix.peaks.iter().flatten().all(|peak| *peak == 0.25));
        assert_eq!(prefix.source_generation, initial.source_generation);
        assert_eq!(prefix.request_epoch, initial.request_epoch);
        assert!(complete.peaks.contains(&Some(0.75)));
        assert_eq!(complete.right_rms_counts.iter().sum::<u64>(), total);
        assert_eq!(complete.rms_counts.iter().sum::<u64>(), total * 2);
        assert_eq!(complete.source_generation, initial.source_generation);
        assert_eq!(complete.request_epoch, initial.request_epoch);
        assert_eq!(
            complete.revision,
            f.client.state.summary.lock().frame.revision
        );
    });
}

#[test]
fn hidden_waveform_collects_without_snapshots_or_wakes_and_show_publishes_immediately() {
    async_io::block_on(async {
        let mut f = fixture();
        let mut track = crate::test_support::artwork_track("hidden.wav".into());
        track.sample_rate = 48_000;
        track.duration = Some(1.0);
        let (state, revision) = subscribe_waveform(&mut f, &track, 1).await;
        f.client.set_waveform_presentation_visible(false);
        let initial = f.client.waveform.read().clone();
        let (wake, wakes) = async_channel::unbounded();
        let staging = Arc::clone(&f.client.analysis);
        f.client.set_wakeup(Arc::new(move || {
            let _ = wake.try_send(staging.read().display_token);
        }));
        // Finish initialization through the installed callback before counting
        // hidden PCM wakes; merely reading staging can race its final wake.
        f.client.set_display_demand(DisplayDemand {
            spectrum: true,
            ..Default::default()
        });
        let display_token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while wakes.recv().await.unwrap() != display_token {}
        })
        .await
        .unwrap();

        stereo(&f.window, header(state, 0), 0.5, 256);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        let first = summary_after(&f, initial.revision).await;
        assert_eq!(first.left_rms_counts.iter().sum::<u64>(), 256);
        let sequence = f.window.sequence();
        stereo(&f.window, header(state, 256), 0.25, 128);
        assert_eq!(f.window.sequence(), sequence);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        let hidden = summary_after(&f, first.revision).await;
        assert_eq!(hidden.left_rms_counts.iter().sum::<u64>(), 384);
        assert_eq!(hidden.request_epoch, initial.request_epoch);
        assert_eq!(hidden.source_generation, initial.source_generation);
        assert_eq!(f.client.waveform.read().revision, initial.revision);
        assert!(f.client.waveform.read().peaks.iter().all(Option::is_none));
        assert_eq!(f.client.state.desired.lock().revision, revision);
        assert_eq!(f.client.state.desired.lock().display_token, display_token);

        // A token-only publication is a worker barrier after both hidden
        // ingests. Any waveform wake would precede it with the old token.
        f.client.set_display_demand(DisplayDemand::default());
        let token = f.client.state.desired.lock().display_token;
        let observed = rivu_protocol::transport::timeout(Duration::from_secs(5), wakes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            observed, token,
            "hidden PCM must not wake waveform presentation"
        );

        f.client.set_waveform_presentation_visible(true);
        let published = f.client.waveform.read();
        assert_eq!(published.revision, hidden.revision);
        assert_eq!(published.peaks, hidden.peaks);
        assert_eq!(published.left_rms_counts, hidden.left_rms_counts);
        assert_eq!(published.request_epoch, initial.request_epoch);
        drop(published);
        assert_eq!(wakes.try_recv(), Ok(token));
        f.client.set_waveform_presentation_visible(true);
        assert!(
            wakes.try_recv().is_err(),
            "unchanged visibility must not wake"
        );
        assert_eq!(f.client.state.desired.lock().revision, revision);
        assert_eq!(f.client.state.desired.lock().display_token, token);
        assert!(f.client.analysis.read().settings.is_none());
    });
}

#[test]
fn hidden_waveform_preserves_analysis_wakes_media_positions_and_source_fences() {
    async_io::block_on(async {
        let mut f = fixture();
        let mut track = crate::test_support::artwork_track("first.wav".into());
        track.sample_rate = 48_000;
        track.duration = Some(1.0);
        let (state, revision) = subscribe_waveform(&mut f, &track, 1).await;
        f.client.set_waveform_presentation_visible(false);
        f.client.set_display_demand(DisplayDemand {
            enabled: true,
            spectrum: true,
            ..Default::default()
        });
        stereo(&f.window, header(state, 0), 0.25, 1024);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        let first = current_after(&f, 0).await;
        assert_eq!(
            first.rms_left, 0.25,
            "analysis still publishes and wakes while hidden"
        );
        assert!(f.client.waveform.read().peaks.iter().all(Option::is_none));
        let request_epoch = f.client.waveform.read().request_epoch;

        let seek = PcmState {
            epoch: 4,
            intent: 1,
            applied_intent: 1,
            ..state
        };
        stereo(&f.window, header(seek, 0), 0.75, 512);
        write_status(&mut f.server, seek, revision, PcmPosition::default())
            .await
            .unwrap();
        let sought = current_after(&f, first.sequence).await;
        assert_eq!(sought.audio_epoch, seek.epoch);
        f.client.set_waveform_presentation_visible(true);
        {
            let waveform = f.client.waveform.read();
            assert_eq!(waveform.peaks.first(), Some(&Some(0.75)));
            assert!(
                waveform.peaks.contains(&Some(0.25)),
                "seek retains unvisited media positions"
            );
            assert!(waveform.left_rms_counts.iter().sum::<u64>() <= 1024);
            assert_eq!(
                waveform.request_epoch, request_epoch,
                "visibility does not cancel full scans"
            );
        }

        f.client.set_waveform_presentation_visible(false);
        track.path = "second.wav".into();
        f.client.configure_source(Some(&track), 2, track.duration);
        assert_eq!(f.client.waveform.read().source_generation, Some(2));
        assert!(f.client.waveform.read().peaks.iter().all(Option::is_none));
        let request = ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        let ControlRequest::Subscribe { revision, .. } = ControlRequest::decode(&request).unwrap()
        else {
            panic!("expected subscription");
        };
        let next = PcmState {
            epoch: 6,
            generation: 2,
            ..seek
        };
        write_status(&mut f.server, next, revision, PcmPosition::default())
            .await
            .unwrap();
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().audio_epoch != next.epoch {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        stereo(
            &f.window,
            header(
                PcmState {
                    generation: 1,
                    ..next
                },
                0,
            ),
            0.875,
            512,
        );
        stereo(&f.window, header(next, 0), 0.5, 512);
        write_status(&mut f.server, next, revision, PcmPosition::default())
            .await
            .unwrap();
        let replacement = current_after(&f, sought.sequence).await;
        assert_eq!(replacement.rms_left, 0.5);
        f.client.set_waveform_presentation_visible(true);
        let waveform_revision = {
            let waveform = f.client.waveform.read();
            assert_eq!(waveform.source_generation, Some(2));
            assert_eq!(waveform.path.as_deref(), Some(track.path.as_path()));
            assert_eq!(waveform.left_rms_counts.iter().sum::<u64>(), 512);
            assert!(waveform.peaks.iter().flatten().all(|peak| *peak == 0.5));
            waveform.revision
        };

        // Removing the configured waveform stops collection even if another
        // analysis consumer keeps the shared PCM subscription alive.
        f.client.set_waveform_presentation_visible(false);
        f.client.set_waveform_demand(false);
        stereo(&f.window, header(next, 512), 0.875, 512);
        write_status(&mut f.server, next, revision, PcmPosition::default())
            .await
            .unwrap();
        let analysis = current_after(&f, replacement.sequence).await;
        assert_eq!(analysis.rms_left, 0.875);
        assert_eq!(
            f.client.state.summary.lock().frame.revision,
            waveform_revision
        );
        f.client.set_waveform_presentation_visible(true);
        assert_eq!(f.client.waveform.read().revision, waveform_revision);
    });
}

#[test]
fn hidden_waveform_master_disable_and_shutdown_fence_collection_and_full_scans() {
    async_io::block_on(async {
        let mut f = fixture();
        let mut track = crate::test_support::artwork_track("disabled.wav".into());
        track.sample_rate = 48_000;
        track.duration = Some(1.0);
        let (state, revision) = subscribe_waveform(&mut f, &track, 1).await;
        f.client.set_waveform_presentation_visible(false);
        let initial = f.client.waveform.read().clone();
        stereo(&f.window, header(state, 0), 0.5, 512);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();
        let hidden = summary_after(&f, initial.revision).await;

        f.client.configure(&Config {
            visualization_enabled: false,
            ..Config::default()
        });
        assert_eq!(
            f.client.waveform.read().request_epoch,
            hidden.request_epoch.wrapping_add(1)
        );
        let request = ControlReader::<REQUEST_SIZE>::new()
            .read(&mut f.server)
            .await
            .unwrap();
        assert_eq!(
            request[1], 0,
            "master disable releases the PCM subscription"
        );
        let ControlRequest::Subscribe { revision, .. } = ControlRequest::decode(&request).unwrap()
        else {
            panic!("expected subscription");
        };
        let token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().display_token != token {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        stereo(&f.window, header(state, 512), 0.875, 512);
        write_status(&mut f.server, state, revision, PcmPosition::default())
            .await
            .unwrap();

        // A disabled demand's token publication proves an ingestion turn after
        // the new PCM prefix, without re-enabling capture.
        f.client.set_display_demand(DisplayDemand {
            spectrum: true,
            ..Default::default()
        });
        let token = f.client.state.desired.lock().display_token;
        rivu_protocol::transport::timeout(Duration::from_secs(5), async {
            while f.client.analysis.read().display_token != token {
                f.wakes.recv().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(
            f.client
                .state
                .summary
                .lock()
                .frame
                .left_rms_counts
                .iter()
                .sum::<u64>(),
            512
        );
        f.client.set_waveform_presentation_visible(true);
        assert_eq!(f.client.waveform.read().revision, hidden.revision);
        assert_eq!(
            f.client.waveform.read().left_rms_counts.iter().sum::<u64>(),
            512
        );

        let request_epoch = f.client.waveform.read().request_epoch;
        f.client.stop();
        assert!(f.client.state.summary.lock().frame.path.is_none());
        assert!(f.client.waveform.read().path.is_none());
        assert_ne!(f.client.waveform.read().request_epoch, request_epoch);
        f.client.set_waveform_presentation_visible(false);
        f.client.set_waveform_presentation_visible(true);
        assert!(f.client.waveform.read().peaks.is_empty());
        assert!(
            !f.client
                .consume_current_analysis(|_| panic!("stopped client accepted analysis"))
        );
    });
}

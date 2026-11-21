use super::*;

fn drain(source: &mut MidiSource, target: u64) -> Vec<f32> {
    let mut all = Vec::new();
    let mut pcm = Vec::new();
    for _ in 0..10_000 {
        match source.next(&mut pcm, target) {
            Step::Pcm(_) => all.extend_from_slice(&pcm),
            Step::Progress => (),
            Step::Eof => return all,
        }
    }
    panic!("MIDI traversal did not finish in bounded rounds")
}

#[test]
fn real_synth_seeks_match_continuous_held_notes_and_sustain() {
    let bytes = fixture::held_note();
    let mut baseline = fixture::source(&bytes);
    let expected = drain(&mut baseline, 0);
    assert_eq!(expected.len(), 96_000 * 2);
    assert!(
        expected[..48_000 * 2]
            .iter()
            .any(|sample| sample.abs() > 0.01)
    );
    assert!(
        expected[48_000 * 2..49_000 * 2]
            .iter()
            .any(|sample| sample.abs() > 0.001)
    );

    let mut source = fixture::source(&bytes);
    let mut pcm = Vec::new();
    assert!(matches!(source.next(&mut pcm, 0), Step::Pcm(0)));
    for target in [12_013, 30_017, 4_019, 36_023, 24_029, 48_031] {
        source.prepare_seek(target).unwrap();
        let mut found = false;
        for _ in 0..1000 {
            match source.next(&mut pcm, target) {
                Step::Pcm(first) => {
                    assert_eq!(first, target);
                    assert_eq!(
                        pcm,
                        expected[target as usize * 2..target as usize * 2 + pcm.len()]
                    );
                    found = true;
                    break;
                }
                Step::Progress => (),
                Step::Eof => panic!("seek reached EOF early"),
            }
        }
        assert!(found, "seek did not converge in bounded rounds");
    }
    let cursor = source.position as usize;
    assert_eq!(drain(&mut source, 0), expected[cursor * 2..]);
}

#[test]
fn same_frame_event_rounds_never_emit_early_audio_or_early_tail() {
    let note = &[0x90, 60, 100][..];
    let controller = &[0xb0, 11, 127][..];
    let end = &[0xff, 0x2f, 0][..];
    let sparse = fixture::smf(&[(1, note), (479, note), (0, end)]);
    let mut events = vec![(1, controller)];
    events.extend(std::iter::repeat_n((0, controller), 599));
    events.push((0, note));
    events.push((479, controller));
    events.extend(std::iter::repeat_n((0, controller), 599));
    events.push((0, note));
    events.push((0, end));
    let mut dense = fixture::source(&fixture::smf(&events));
    let expected = drain(&mut fixture::source(&sparse), 0);
    let mut pcm = Vec::new();
    assert!(matches!(dense.next(&mut pcm, 0), Step::Pcm(0)));
    assert_eq!(pcm, vec![0.; 64 * 2]);
    assert_eq!(dense.position, 64);
    for _ in 0..2 {
        assert!(matches!(dense.next(&mut pcm, 0), Step::Progress));
        assert_eq!(dense.position, 64);
    }
    let mut actual = vec![0.; 64 * 2];
    actual.extend(drain(&mut dense, 0));
    assert_eq!(actual, expected);
    assert_eq!(dense.position, 24_000 + TAIL);
    assert!(matches!(dense.next(&mut pcm, 0), Step::Eof));
}

#[test]
fn identity_change_is_a_source_failure_but_cancellation_is_not() {
    let bytes = fixture::held_note();
    let identity = |reader: &std::io::Cursor<&[u8]>| {
        Ok(rivu_protocol::audio::SourceIdentity::File {
            device: 1,
            inode: 2,
            len: reader.get_ref().len() as u64,
            modified: (reader.position() as i64, 0),
            changed: (0, 0),
        })
    };
    let mut reader = std::io::Cursor::new(bytes.as_slice());
    let error = read_checked(&mut reader, None, None, identity)
        .err()
        .unwrap();
    assert!(crate::source::is_source_failure(&error));
    assert!(format!("{error:#}").contains("changed while reading"));
    let cancelled = AtomicBool::new(true);
    let mut reader = std::io::Cursor::new(bytes.as_slice());
    let error = read_checked(&mut reader, None, Some(&cancelled), identity)
        .err()
        .unwrap();
    assert!(!crate::source::is_source_failure(&error));
    assert_eq!(reader.position(), 0);

    // Cancellation during the final lookup wins over the changed identity.
    let cancelled = AtomicBool::new(false);
    let mut reader = std::io::Cursor::new(bytes.as_slice());
    let error = read_checked(&mut reader, None, Some(&cancelled), |reader| {
        if reader.position() > 0 {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        identity(reader)
    })
    .err()
    .unwrap();
    assert!(!crate::source::is_source_failure(&error));
}

#[test]
fn end_of_track_releases_a_pedal_held_note_like_an_explicit_release() {
    let implicit = drain(&mut fixture::source(&fixture::held_note()), 0);
    let explicit = fixture::held_note_with_release(true);
    assert_eq!(implicit, drain(&mut fixture::source(&explicit), 0));
}

#[test]
fn bank_load_publishes_one_immutable_success_or_failure() {
    let bank = fixture::bank();
    let expected = Arc::clone(&bank);
    let load = SoundFontLoad::start(move || Ok(bank));
    let _ = load.ready.recv();
    for _ in 0..3 {
        assert!(Arc::ptr_eq(load.poll().unwrap().unwrap(), &expected));
    }
    let failure = SoundFontLoad::start(|| anyhow::bail!("deterministic bank failure"));
    let _ = failure.ready.recv();
    for _ in 0..3 {
        assert!(
            failure
                .poll()
                .unwrap_err()
                .to_string()
                .contains("deterministic bank failure")
        );
    }
}

#[test]
fn probe_and_render_share_timing_validation_and_ignored_messages() {
    let note = &[0x90, 60, 100][..];
    let off = &[0x80, 60, 0][..];
    let end = &[0xff, 0x2f, 0][..];
    let plain = fixture::smf(&[(0, note), (480, off), (480, end)]);
    let bytes = fixture::smf(&[
        (0, &[0xff, 3, 5, b'T', b'i', b't', b'l', b'e']),
        (0, &[0xf0, 3, 0x7d, 1, 0xf7]),
        (0, &[0xa0, 60, 127]),
        (0, &[0xd0, 127]),
        (0, note),
        (480, off),
        (480, end),
    ]);
    let mut retained = Vec::new();
    let timeline = Timeline::parse(&bytes, Some(&mut retained), None).unwrap();
    let probe = Timeline::parse(&bytes, None, None)
        .unwrap()
        .info(Path::new("fixture.mid"));
    assert_eq!(retained.len(), 2);
    assert_eq!(timeline.end, 96_000);
    assert_eq!(probe.duration, Some(2.0));
    assert_eq!(probe.title, "Title");
    assert_eq!((probe.sample_rate, probe.channels), (0, 0));
    assert_eq!(
        drain(&mut fixture::source(&bytes), 0),
        drain(&mut fixture::source(&plain), 0)
    );

    let mut sequential = bytes;
    sequential[9] = 2;
    for retain in [false, true] {
        let mut events = Vec::new();
        let error = Timeline::parse(&sequential, retain.then_some(&mut events), None)
            .err()
            .unwrap();
        assert!(error.to_string().contains("format 2"));
    }
}

#[test]
fn same_tick_messages_retain_file_order() {
    let bytes = fixture::smf(&[
        (0, &[0x90, 60, 100]),
        (0, &[0x80, 60, 0]),
        (0, &[0x90, 60, 80]),
        (480, &[0xff, 0x2f, 0]),
    ]);
    let mut events = Vec::new();
    Timeline::parse(&bytes, Some(&mut events), None).unwrap();
    assert_eq!(events.len(), 3);
    assert!(
        matches!(events[0], (0, Event::Midi(0, MidiMessage::NoteOn { vel, .. })) if vel.as_int() == 100)
    );
    assert!(matches!(
        events[1],
        (0, Event::Midi(0, MidiMessage::NoteOff { .. }))
    ));
    assert!(
        matches!(events[2], (0, Event::Midi(0, MidiMessage::NoteOn { vel, .. })) if vel.as_int() == 80)
    );
}

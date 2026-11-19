use crate::i18n::Localizer;
use rivu_format::config::ReplayGainPeakPolicy;
use rivu_protocol::model::AudioOutputState;

/// Shared text presentation; all audio facts come from the runtime snapshot.
pub fn output_rows(state: &AudioOutputState, i18n: &Localizer) -> Vec<(String, String)> {
    let mut rows = Vec::with_capacity(20);
    visit_output_fields(state, i18n, |key, value| {
        rows.push((i18n.text(key).to_owned(), value));
    });
    rows
}

/// Visit formatted values with their localization keys, without imposing a layout.
pub fn visit_output_fields(
    state: &AudioOutputState,
    i18n: &Localizer,
    mut add: impl FnMut(&'static str, String),
) {
    let unknown = i18n.text("audio-unknown");
    let on_off = |on| i18n.text(if on { "audio-on" } else { "audio-off" });
    let rate = |value: Option<u32>| value.map_or_else(|| unknown.to_owned(), |v| format!("{v} Hz"));
    let db =
        |value: Option<f64>| value.map_or_else(|| unknown.to_owned(), |v| format!("{v:+.2} dB"));
    let peak = |value: Option<f64>| match value {
        Some(value) if value > 0.0 => format!("{:.2} dBFS", 20.0 * value.log10()),
        Some(_) => i18n.text("audio-silence").to_owned(),
        None => "-".to_owned(),
    };
    let ms = |frames: u64| frames as f64 * 1000.0 / f64::from(state.stream.sample_rate.max(1));
    let metrics = &state.metrics;
    let gain = &state.replaygain;
    add(
        "audio-source",
        format!(
            "{} · {} · {} Hz · {} · {}",
            i18n.text(if state.source.processed {
                "audio-processed"
            } else {
                "audio-original"
            }),
            state.source.codec,
            state.source.sample_rate,
            state.source.channels.join(", "),
            state
                .source
                .bits_per_sample
                .map_or_else(|| unknown.to_owned(), |v| format!("{v} bit"))
        ),
    );
    add(
        "audio-stream",
        format!(
            "{} Hz · {} · {}",
            state.stream.sample_rate,
            state.stream.sample_format,
            state.stream.channels.join(", ")
        ),
    );
    add(
        "audio-sink",
        state.sink.as_deref().unwrap_or(unknown).to_owned(),
    );
    add(
        "audio-device",
        format!(
            "{} · {}",
            rate(state.device_rate),
            state.device_format.as_deref().unwrap_or(unknown)
        ),
    );
    add(
        "audio-graph-clock",
        format!(
            "{} · {}",
            rate(state.graph_rate),
            state.quantum_frames.map_or_else(
                || unknown.to_owned(),
                |frames| match state.graph_rate.filter(|&rate| rate > 0) {
                    Some(rate) => format!(
                        "{frames} frames ({:.2} ms)",
                        f64::from(frames) * 1000.0 / f64::from(rate)
                    ),
                    None => format!("{frames} frames"),
                },
            )
        ),
    );
    let stream_rate = state.stream.sample_rate;
    add(
        "audio-stream-rate-conversion",
        match state.graph_rate {
            Some(graph_rate) if graph_rate == stream_rate => {
                i18n.text("audio-matching-rates").to_owned()
            }
            Some(graph_rate) => format!("{stream_rate} → {graph_rate} Hz"),
            None => unknown.to_owned(),
        },
    );
    add(
        "audio-stream-resample-quality",
        state
            .stream_resample_quality
            .map_or_else(|| unknown.to_owned(), |quality| quality.to_string()),
    );
    add("audio-remix", on_off(state.remix_allowed).to_owned());
    add(
        "audio-processing",
        format!(
            "Graph: {} · Monitor EQ: {}",
            on_off(state.graph_processing),
            on_off(state.monitor_eq)
        ),
    );
    add(
        "audio-master-gain",
        state
            .master_gain_db
            .map_or_else(|| i18n.text("audio-silence").to_owned(), |v| db(Some(v))),
    );
    add(
        "audio-replaygain",
        format!(
            "{} · {} → {} · {}: {}",
            on_off(gain.enabled),
            db(gain.requested_gain_db),
            db(Some(gain.applied_gain_db)),
            i18n.text("audio-peak-policy"),
            on_off(gain.peak_policy == ReplayGainPeakPolicy::PreventClipping)
        ),
    );
    add(
        "audio-peaks",
        format!(
            "{} / {} / {}",
            peak(metrics.sample_peak.map(f64::from)),
            peak(metrics.sample_peak_hold.map(f64::from)),
            if gain.processed_source {
                i18n.text("audio-processed-peak-unknown").to_owned()
            } else {
                peak(gain.source_peak)
            }
        ),
    );
    add(
        "audio-source-loudness",
        match state.source_loudness_lufs.filter(|value| value.is_finite()) {
            Some(lufs) => format!("{lufs:.2} LUFS / {:+.2} dB", -18.0 - lufs),
            None => "- / -".to_owned(),
        },
    );
    add("audio-overs", metrics.over_range_frames.to_string());
    add(
        "audio-buffer",
        format!(
            "{:.2} / {} ms",
            ms(metrics.buffered_frames),
            metrics
                .low_water_frames
                .map_or_else(|| unknown.to_owned(), |v| format!("{:.2}", ms(v)))
        ),
    );
    add(
        "audio-wait",
        format!(
            "{:.2} / {:.2} ms",
            ms(metrics.pending_missing_frames),
            ms(metrics.startup_wait_frames)
        ),
    );
    add(
        "audio-underruns",
        format!(
            "{} · {:.2} ms · {} callbacks",
            metrics.underrun_events,
            ms(metrics.missing_frames),
            metrics.underrun_callbacks
        ),
    );
    add(
        "audio-session-underruns",
        format!(
            "{} · {:.2} ms",
            state.session_underruns,
            state.session_missing_seconds * 1000.0
        ),
    );
    add(
        "audio-last-underrun",
        metrics
            .last_underrun_ago_seconds
            .map_or_else(|| unknown.to_owned(), |v| format!("{v:.1} s")),
    );
    add(
        "audio-output-state",
        format!(
            "{} · {} / {}",
            i18n.text(if state.active {
                "audio-active"
            } else {
                "audio-retired"
            }),
            state.source_generation,
            state.segment
        ),
    );
}

/// Format the complete snapshot on demand, including rows outside the viewport.
pub fn output_text(state: Option<&AudioOutputState>, i18n: &Localizer) -> String {
    let mut text = i18n.text("audio-output").to_owned();
    text.push('\n');
    if let Some(state) = state {
        for (label, value) in output_rows(state, i18n) {
            text.push_str(&label);
            text.push_str(": ");
            text.push_str(&value);
            text.push('\n');
        }
        text.push('\n');
        text.push_str(i18n.text("audio-output-note"));
    } else {
        text.push_str(i18n.text("audio-output-empty"));
    }
    text
}

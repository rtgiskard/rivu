use crate::i18n::Localizer;
use rivu_format::config::ReplayGainPeakPolicy;
use rivu_protocol::model::AudioOutputState;

/// Shared text presentation; all audio facts come from the runtime snapshot.
pub fn output_rows(state: &AudioOutputState, i18n: &Localizer) -> Vec<(String, String)> {
    let unknown = i18n.text("audio-unknown");
    let on_off = |on| i18n.text(if on { "audio-on" } else { "audio-off" });
    let rate = |value: Option<u32>| value.map_or_else(|| unknown.to_owned(), |v| format!("{v} Hz"));
    let db =
        |value: Option<f64>| value.map_or_else(|| unknown.to_owned(), |v| format!("{v:+.2} dB"));
    let peak = |value: Option<f32>| match value {
        Some(value) if value > 0.0 => format!("{:.2} dBFS", 20.0 * f64::from(value).log10()),
        Some(_) => i18n.text("audio-silence").to_owned(),
        None => unknown.to_owned(),
    };
    let ms = |frames: u64| frames as f64 * 1000.0 / f64::from(state.stream.sample_rate.max(1));
    let metrics = &state.metrics;
    let gain = &state.replaygain;
    let mut rows = Vec::with_capacity(24);
    let mut add = |key: &str, value: String| rows.push((i18n.text(key).to_owned(), value));
    add(
        "audio-source",
        format!(
            "{} · {} Hz · {} · {}",
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
        "audio-source-kind",
        i18n.text(if state.source.processed {
            "audio-processed"
        } else {
            "audio-original"
        })
        .to_owned(),
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
    add("audio-remix", on_off(state.remix_allowed).to_owned());
    add(
        "audio-graph-clock",
        format!(
            "{} · {}",
            rate(state.graph_rate),
            state
                .quantum_frames
                .map_or_else(|| unknown.to_owned(), |v| format!("{v} frames"))
        ),
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
            "{} · {} → {}",
            on_off(gain.enabled),
            db(gain.requested_gain_db),
            db(Some(gain.applied_gain_db))
        ),
    );
    add(
        "audio-peak-policy",
        i18n.text(match gain.peak_policy {
            ReplayGainPeakPolicy::Off => "audio-off",
            ReplayGainPeakPolicy::PreventClipping => "audio-peak-prevent",
        })
        .to_owned(),
    );
    add(
        "audio-source-peak",
        if gain.processed_source {
            i18n.text("audio-processed-peak-unknown").to_owned()
        } else {
            gain.source_peak.map_or_else(
                || unknown.to_owned(),
                |v| format!("{:.2} dBFS", 20.0 * v.log10()),
            )
        },
    );
    add("audio-sample-peak", peak(metrics.sample_peak));
    add("audio-peak-hold", peak(metrics.sample_peak_hold));
    add(
        "audio-headroom",
        match metrics.sample_peak {
            Some(v) if v > 0.0 => format!("{:.2} dB", -20.0 * f64::from(v).log10()),
            Some(_) => i18n.text("audio-silence").to_owned(),
            None => unknown.to_owned(),
        },
    );
    add("audio-overs", metrics.over_range_frames.to_string());
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
        "audio-supply-wait",
        format!("{:.2} ms", ms(metrics.pending_missing_frames)),
    );
    add(
        "audio-startup-wait",
        format!("{:.2} ms", ms(metrics.startup_wait_frames)),
    );
    add(
        "audio-buffer",
        format!("{:.2} ms", ms(metrics.buffered_frames)),
    );
    add(
        "audio-buffer-low",
        metrics
            .low_water_frames
            .map_or_else(|| unknown.to_owned(), |v| format!("{:.2} ms", ms(v))),
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
    rows
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

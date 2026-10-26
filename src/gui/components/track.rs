use crate::gui::appearance::fonts;
use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, rgb};
use rivu_ui::{caption, column, lazy_row_tooltip, list_row, row, row_tooltip, two_line_text};
use std::{borrow::Cow, fmt::Write, path::Path};

pub(in crate::gui) fn track_title<'a>(
    title: &'a str,
    path: &'a Path,
    cue_number: Option<u32>,
) -> Cow<'a, str> {
    if title.trim().is_empty() {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        match cue_number {
            Some(number) => Cow::Owned(format!("{stem} · #{number:02}")),
            None => stem,
        }
    } else {
        Cow::Borrowed(title)
    }
}

/// Cached source facts only; formatting never opens media or queries the library.
#[derive(Clone, Copy, Default)]
pub(in crate::gui) struct AudioSpecs<'a> {
    pub codec: &'a str,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    pub channels: u16,
    pub bitrate_bps: Option<u64>,
    pub bitrate_exact: bool,
}

impl<'a> From<&'a rivu_protocol::model::LibraryRow> for AudioSpecs<'a> {
    fn from(track: &'a rivu_protocol::model::LibraryRow) -> Self {
        Self {
            codec: &track.codec,
            sample_rate: track.sample_rate,
            bits_per_sample: track.bits_per_sample,
            channels: track.channels,
            bitrate_bps: track.bitrate_bps,
            bitrate_exact: track.bitrate_exact,
        }
    }
}

pub(in crate::gui) fn audio_specs(specs: AudioSpecs<'_>) -> String {
    let mut text = String::new();
    let mut part = |value: &dyn std::fmt::Display| {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        let _ = write!(text, "{value}");
    };
    if !specs.codec.trim().is_empty() {
        part(&specs.codec);
    }
    if specs.sample_rate > 0 {
        part(&format_args!("{} Hz", specs.sample_rate));
    }
    // This field is source PCM/lossless precision, not decoder output width.
    if let Some(bits) = specs.bits_per_sample.filter(|bits| *bits > 0) {
        part(&format_args!("{bits} bit"));
    }
    if specs.channels > 0 {
        part(&format_args!("{} ch", specs.channels));
    }
    if let Some(bitrate) = specs.bitrate_bps.filter(|bitrate| *bitrate > 0) {
        part(&format_args!(
            "{}{:.0} kbps",
            if specs.bitrate_exact { "" } else { "≈" },
            bitrate as f64 / 1000.0,
        ));
    }
    text
}

pub(in crate::gui) struct TrackText<'a> {
    pub title: Cow<'a, str>,
    pub artist: &'a str,
    pub album: Option<&'a str>,
    pub duration: Option<f64>,
    pub audio: AudioSpecs<'a>,
}

impl<'a> From<&'a rivu_protocol::model::LibraryRow> for TrackText<'a> {
    fn from(track: &'a rivu_protocol::model::LibraryRow) -> Self {
        Self {
            title: track_title(&track.title, &track.path, track.cue_number),
            artist: &track.artist,
            album: Some(&track.album),
            duration: track.duration,
            audio: AudioSpecs::from(track),
        }
    }
}

impl<'a> From<&'a rivu_protocol::model::PlaylistEntryRow> for TrackText<'a> {
    fn from(track: &'a rivu_protocol::model::PlaylistEntryRow) -> Self {
        Self {
            title: track_title(&track.title, &track.path, track.cue_number),
            artist: &track.artist,
            album: Some(&track.album),
            duration: track.duration,
            audio: AudioSpecs {
                codec: &track.codec,
                sample_rate: track.sample_rate,
                bits_per_sample: track.bits_per_sample,
                channels: track.channels,
                bitrate_bps: track.bitrate_bps,
                bitrate_exact: track.bitrate_exact,
            },
        }
    }
}

fn metadata_text(
    theme: &'static rivu_ui::theme::Palette,
    id: ElementId,
    text: &str,
    symbol: &'static str,
    missing_label: &str,
) -> gpui::AnyElement {
    if text.trim().is_empty() {
        // Caption text is 0.75rem: reserve exactly one em for the symbol.
        div()
            .id(id)
            .w(gpui::rems(0.75))
            .flex_shrink_0()
            .text_center()
            .font_family(fonts::SYMBOLS_FAMILY)
            .font_size_scale(1.)
            .tooltip(rivu_ui::text_tooltip(missing_label.to_owned()))
            .child(symbol)
            .into_any_element()
    } else {
        caption(theme, text.to_owned())
            .min_w_0()
            .truncate()
            .into_any_element()
    }
}
/// Numbered track row shared by album, playlist, and queue views.
pub(crate) fn track_list_row(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<gpui::ElementId>,
    text: TrackText<'_>,
    index: usize,
    playing: bool,
    selected: bool,
    i18n: &crate::i18n::Localizer,
) -> gpui::Stateful<Div> {
    let title: SharedString = text.title.into_owned().into();
    let mut detail = row()
        .min_w_0()
        .gap_1()
        .text_xs()
        .text_color(rgb(theme.secondary))
        .child(metadata_text(
            theme,
            "track-missing-artist".into(),
            text.artist,
            "󰠃",
            i18n.text("gl-artist-missing"),
        ));
    if let Some(album) = text.album {
        detail = detail.child("·").child(metadata_text(
            theme,
            "track-missing-album".into(),
            album,
            "󰀥",
            i18n.text("gl-album-missing"),
        ));
    }
    let codec = text.audio.codec.to_owned();
    let AudioSpecs {
        sample_rate,
        bits_per_sample,
        channels,
        bitrate_bps,
        bitrate_exact,
        ..
    } = text.audio;
    let text_view = column()
        .id("track-text")
        .flex_1()
        .min_w_0()
        .gap_0()
        .line_height(gpui::relative(1.28))
        .overflow_hidden()
        .text_color(rgb(if playing { theme.accent } else { theme.text }))
        .child(div().text_sm().truncate().child(title.clone()))
        .child(detail);
    let text_view = lazy_row_tooltip(text_view, move || {
        (
            title.clone(),
            audio_specs(AudioSpecs {
                codec: &codec,
                sample_rate,
                bits_per_sample,
                channels,
                bitrate_bps,
                bitrate_exact,
            })
            .into(),
        )
    });
    list_row(theme, id, selected)
        .child(
            div()
                .w_8()
                .flex_shrink_0()
                .text_xs()
                .whitespace_nowrap()
                .font_features(fonts::tabular_numbers())
                .text_color(rgb(if playing {
                    theme.accent
                } else {
                    theme.secondary
                }))
                .when(playing, |view| {
                    view.font_family(fonts::SYMBOLS_FAMILY).font_size_scale(1.)
                })
                .child(if playing {
                    "".to_owned()
                } else {
                    (index + 1).to_string()
                }),
        )
        .child(text_view)
        .child(
            caption(
                theme,
                text.duration
                    .map_or_else(|| "—".to_owned(), crate::gui::format_time),
            )
            .font_features(fonts::tabular_numbers())
            .w_16()
            .flex_shrink_0()
            .text_right(),
        )
}

/// Standard two-line row text with a full-content hover tooltip.
pub(crate) fn row_text(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    let view = two_line_text(theme, title.clone(), detail.clone()).id(id);
    row_tooltip(view, title, detail)
}

/// Favorite icon and selected accent without a selected background.
pub(in crate::gui) fn favorite_button(
    id: impl Into<ElementId>,
    favorite: bool,
    text: impl Fn(&str) -> SharedString,
) -> rivu_ui::IconButton {
    rivu_ui::IconButton::new(
        id,
        if favorite { "󰓎" } else { "󰓐" },
        text(if favorite {
            "gl-unfavorite"
        } else {
            "gl-favorite"
        }),
    )
    .toggled(favorite)
    .selected_fill(false)
}

/// Favorite action; the caller owns visibility, commands, and current state.
pub(in crate::gui) fn track_favorite_toggle<T: 'static>(
    id: impl Into<ElementId>,
    favorite: bool,
    density: rivu_ui::IconButtonDensity,
    text: impl Fn(&str) -> SharedString,
    cx: &mut gpui::Context<T>,
    action: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Stateful<Div> {
    let id = id.into();
    rivu_ui::row_action_region(id.clone()).child(favorite_button(id, favorite, text).build(
        rivu_ui::IconButtonStyle::Quiet,
        density,
        cx,
        action,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_fallback_uses_stem_only_for_missing_or_blank_titles() {
        let path = Path::new("/music/夜曲.live.flac");
        for title in ["", " \t\n", "\u{2003}"] {
            assert_eq!(track_title(title, path, None), "夜曲.live");
        }
        let title = "  A complete title  ";
        assert!(
            matches!(track_title(title, path, Some(2)), Cow::Borrowed(value) if value == title)
        );
        assert_eq!(track_title(" ", path, Some(2)), "夜曲.live · #02");
    }

    #[test]
    fn unknown_and_zero_specs_do_not_create_placeholders() {
        assert!(audio_specs(AudioSpecs::default()).is_empty());
        assert!(
            audio_specs(AudioSpecs {
                codec: " \t",
                bits_per_sample: Some(0),
                bitrate_bps: Some(0),
                ..AudioSpecs::default()
            })
            .is_empty()
        );
        let partial = audio_specs(AudioSpecs {
            sample_rate: 48_000,
            ..AudioSpecs::default()
        });
        assert!(partial.contains("48000"));
        assert_eq!(partial.split(" · ").count(), 1);
    }

    #[test]
    fn source_precision_and_bitrate_certainty_are_preserved() {
        let source = AudioSpecs {
            codec: "flac",
            sample_rate: 44_100,
            bits_per_sample: Some(24),
            channels: 2,
            bitrate_bps: Some(900_000),
            bitrate_exact: true,
        };
        let lossless = audio_specs(source);
        assert!(lossless.contains("24 bit"));
        assert!(!lossless.contains('≈'));
        // A lossy source has no source precision; never invent decoder width.
        let lossy = audio_specs(AudioSpecs {
            codec: "mp3",
            bits_per_sample: None,
            bitrate_exact: false,
            ..source
        });
        assert!(!lossy.contains(" bit"));
        assert!(lossy.contains("≈900"));
    }
}

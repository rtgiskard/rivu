//! Speaker negotiation for CPAL. CPAL exposes counts, not speaker positions.
//! On Linux query the same ALSA PCM/configuration before CPAL opens its stream;
//! never interpret a seven-channel count as evidence of a 6.1 speaker layout.

use super::Channel;
use anyhow::{Context, Result, bail};
use cpal::traits::DeviceTrait;

pub(super) fn mapping(source: &[Channel], device: &[Channel]) -> Result<Vec<Option<usize>>> {
    if source.is_empty() || device.is_empty() {
        bail!("Empty source or device speaker layout");
    }
    for (index, channel) in source.iter().enumerate() {
        if source[..index].contains(channel) {
            bail!("Duplicate source speaker: {channel:?}");
        }
    }
    for (index, channel) in device.iter().enumerate() {
        if device[..index].contains(channel) {
            bail!("Duplicate device speaker: {channel:?}");
        }
    }
    // Preserve the established mono-on-stereo behavior, explicitly duplicating
    // a single center signal. No multichannel signal is ever downmixed here.
    if source == [Channel::FrontCenter] && device == [Channel::FrontLeft, Channel::FrontRight] {
        return Ok(vec![Some(0), Some(0)]);
    }
    for channel in source {
        if !device.contains(channel) {
            bail!(
                "Device layout {device:?} lacks source speaker {channel:?} (source {source:?}); refusing downmix"
            );
        }
    }
    Ok(device
        .iter()
        .map(|channel| source.iter().position(|s| s == channel))
        .collect())
}

pub(super) fn negotiate(
    device: &cpal::Device,
    source_rate: u32,
    source: &[Channel],
) -> Result<(cpal::SupportedStreamConfig, Vec<Option<usize>>)> {
    if source_rate == 0 || source.is_empty() {
        bail!("Invalid decoded sample rate or speaker layout");
    }
    let mut candidates: Vec<_> = device
        .supported_output_configs()?
        .filter(|config| usize::from(config.channels()) >= source.len())
        .filter(|config| config.min_sample_rate() > 0)
        .map(|config| {
            let rate = source_rate.clamp(config.min_sample_rate(), config.max_sample_rate());
            config.with_sample_rate(rate)
        })
        .collect();
    candidates.sort_by_key(|config| {
        (
            usize::from(config.channels()) - source.len(),
            config.sample_rate().abs_diff(source_rate),
            match config.sample_format() {
                cpal::SampleFormat::F32 => 0,
                cpal::SampleFormat::F64 => 1,
                cpal::SampleFormat::I32 => 2,
                cpal::SampleFormat::I24 => 3,
                cpal::SampleFormat::I16 => 4,
                _ => 5,
            },
        )
    });
    let mut failures = Vec::new();
    for config in candidates {
        match device_layout(device, &config).and_then(|layout| mapping(source, &layout)) {
            Ok(map) => return Ok((config, map)),
            Err(error) => {
                let reason = format!(
                    "{}ch/{}Hz/{:?}: {error:#}",
                    config.channels(),
                    config.sample_rate(),
                    config.sample_format()
                );
                // Keep diagnostics bounded for plugins advertising many formats.
                if failures.len() < 8 {
                    failures.push(reason);
                }
            }
        }
    }
    bail!(
        "Output device {:?} cannot preserve source layout {source:?} at {source_rate} Hz: {}",
        device.description()?.name(),
        if failures.is_empty() {
            "no configuration with enough output channels".to_owned()
        } else {
            failures.join("; ")
        }
    )
}

fn conventional_layout(channels: u16) -> Result<Vec<Channel>> {
    match channels {
        1 => Ok(vec![Channel::FrontCenter]),
        2 => Ok(vec![Channel::FrontLeft, Channel::FrontRight]),
        _ => bail!("Backend does not expose a verified multichannel speaker map"),
    }
}

#[cfg(not(target_os = "linux"))]
fn device_layout(_: &cpal::Device, config: &cpal::SupportedStreamConfig) -> Result<Vec<Channel>> {
    conventional_layout(config.channels())
}

#[cfg(target_os = "linux")]
fn device_layout(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
) -> Result<Vec<Channel>> {
    use alsa::pcm::{Access, Format, HwParams, PCM};
    let id = device.id()?;
    if id.host() != cpal::HostId::Alsa {
        return conventional_layout(config.channels());
    }
    // RAII closes the probe before the caller builds the CPAL stream, including
    // error paths. This also supports exclusive hardware PCMs.
    let pcm = PCM::new(id.id(), alsa::Direction::Playback, true)
        .with_context(|| format!("Opening ALSA speaker-map probe {}", id.id()))?;
    let params = HwParams::any(&pcm)?;
    params.set_access(Access::RWInterleaved)?;
    let native = match config.sample_format() {
        cpal::SampleFormat::F32 => Format::float(),
        cpal::SampleFormat::F64 => Format::float64(),
        cpal::SampleFormat::I8 => Format::S8,
        cpal::SampleFormat::U8 => Format::U8,
        cpal::SampleFormat::I16 => Format::s16(),
        cpal::SampleFormat::U16 => Format::u16(),
        cpal::SampleFormat::I24 => Format::s24(),
        cpal::SampleFormat::U24 => Format::u24(),
        cpal::SampleFormat::I32 => Format::s32(),
        cpal::SampleFormat::U32 => Format::u32(),
        other => bail!("Cannot probe ALSA sample format {other:?}"),
    };
    let opposite = match native {
        Format::FloatLE => Format::FloatBE,
        Format::FloatBE => Format::FloatLE,
        Format::Float64LE => Format::Float64BE,
        Format::Float64BE => Format::Float64LE,
        Format::S16LE => Format::S16BE,
        Format::S16BE => Format::S16LE,
        Format::U16LE => Format::U16BE,
        Format::U16BE => Format::U16LE,
        Format::S24LE => Format::S24BE,
        Format::S24BE => Format::S24LE,
        Format::U24LE => Format::U24BE,
        Format::U24BE => Format::U24LE,
        Format::S32LE => Format::S32BE,
        Format::S32BE => Format::S32LE,
        Format::U32LE => Format::U32BE,
        Format::U32BE => Format::U32LE,
        format => format,
    };
    params.set_format(if params.test_format(native).is_ok() {
        native
    } else {
        opposite
    })?;
    params.set_rate(config.sample_rate(), alsa::ValueOr::Nearest)?;
    params.set_channels(u32::from(config.channels()))?;
    pcm.hw_params(&params)?;
    if params.get_rate()? != config.sample_rate() {
        bail!("ALSA changed the requested sample rate");
    }
    let map = match pcm.get_chmap() {
        Ok(map) => map,
        Err(_) if config.channels() <= 2 => return conventional_layout(config.channels()),
        Err(error) => bail!(
            "ALSA PCM {} cannot report its speaker map: {error}; select a PCM with channel-map support (for example PipeWire ALSA)",
            id.id()
        ),
    };
    // ALSA's textual names also preserve flags/unknown positions. Unlike
    // alsa-rs's Vec conversion, parsing them cannot panic on flagged positions.
    let text = map.to_string();
    let layout = parse_alsa_map(&text)?;
    if layout.len() != usize::from(config.channels()) {
        bail!(
            "ALSA reported {} speakers for {} channels",
            layout.len(),
            config.channels()
        );
    }
    Ok(layout)
}

#[cfg(target_os = "linux")]
fn parse_alsa_map(text: &str) -> Result<Vec<Channel>> {
    use Channel::*;
    text.split_whitespace()
        .map(|name| {
            Ok(match name {
                "MONO" | "FC" => FrontCenter,
                "FL" => FrontLeft,
                "FR" => FrontRight,
                "LFE" => Lfe,
                "RL" => RearLeft,
                "RR" => RearRight,
                "RC" => RearCenter,
                "SL" => SideLeft,
                "SR" => SideRight,
                "FLC" => FrontLeftCenter,
                "FRC" => FrontRightCenter,
                "TC" => TopCenter,
                "TFL" => TopFrontLeft,
                "TFC" => TopFrontCenter,
                "TFR" => TopFrontRight,
                "TRL" => TopRearLeft,
                "TRC" => TopRearCenter,
                "TRR" => TopRearRight,
                _ => bail!("Unsupported or unspecified ALSA speaker {name:?} in map {text:?}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::*;

    #[test]
    fn mapping_preserves_every_speaker_and_never_guesses_from_count() {
        for source in [
            vec![FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight,
            ],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearLeft,
                RearRight,
                SideLeft,
                SideRight,
            ],
        ] {
            let mut device = source.clone();
            device.rotate_left(2);
            let map = mapping(&source, &device).unwrap();
            for (destination, index) in map.iter().enumerate() {
                assert_eq!(source[index.unwrap()], device[destination]);
            }
        }
        assert!(
            mapping(
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    Lfe,
                    RearCenter,
                    SideLeft,
                    SideRight
                ],
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    Lfe,
                    RearLeft,
                    RearRight,
                    TopCenter
                ]
            )
            .is_err()
        );
        assert!(mapping(&[FrontLeft, FrontRight, Lfe], &[FrontLeft, FrontRight]).is_err());
        assert_eq!(
            mapping(&[FrontCenter], &[FrontLeft, FrontRight]).unwrap(),
            [Some(0), Some(0)]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn alsa_maps_are_parsed_by_position_not_count() {
        assert_eq!(
            parse_alsa_map("FL FR RL RR FC LFE").unwrap(),
            [FrontLeft, FrontRight, RearLeft, RearRight, FrontCenter, Lfe]
        );
        assert_eq!(
            parse_alsa_map("FL FR FC LFE RC SL SR").unwrap(),
            [
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight
            ]
        );
        assert!(parse_alsa_map("FL FR UNKNOWN").is_err());
        assert!(parse_alsa_map("FL FR NA").is_err());
    }
}

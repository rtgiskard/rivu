use anyhow::{Result, ensure};
#[cfg(not(feature = "ffmpeg"))]
use rivu_protocol::model::AudioExportFormat;

#[cfg(feature = "ffmpeg")]
pub use crate::ffmpeg::encode::{AudioEncoder, ExportConverter, export_encoder_available};

#[cfg(not(feature = "ffmpeg"))]
pub fn export_encoder_available(format: AudioExportFormat) -> bool {
    format == AudioExportFormat::Wav
}

pub fn validate_integer_samples(samples: &[f32], bits: u16) -> Result<()> {
    let maximum = 1.0 - 2_f64.powi(1 - i32::from(bits));
    ensure!(
        samples
            .iter()
            .all(|&x| x.is_finite() && f64::from(x) >= -1.0 && f64::from(x) <= maximum),
        "Integer export would clip; reduce explicit EQ output gain or choose float32 WAV"
    );
    Ok(())
}

#[cfg(not(feature = "ffmpeg"))]
mod unavailable {
    use anyhow::{Result, bail};
    use rivu_protocol::model::AudioExportEncoding;
    use std::path::Path;
    pub struct AudioEncoder;
    impl AudioEncoder {
        pub fn new(_: &Path, _: u32, _: u64, _: &AudioExportEncoding) -> Result<Self> {
            bail!("Encoded export requires the ffmpeg build feature")
        }
        pub fn write(&mut self, _: &[f32]) -> Result<()> {
            bail!("Encoded export is unavailable")
        }
        pub fn finish(&mut self) -> Result<()> {
            bail!("Encoded export is unavailable")
        }
    }
    pub struct ExportConverter;
    impl ExportConverter {
        pub fn new(_: u32, _: u64, _: u32, _: u64) -> Result<Self> {
            bail!("Sample rate/channel conversion requires the ffmpeg build feature")
        }
        pub fn convert(&mut self, _: &[f32]) -> Result<&[f32]> {
            bail!("Export conversion is unavailable")
        }
        pub fn finish(&mut self) -> Result<&[f32]> {
            bail!("Export conversion is unavailable")
        }
    }
}
#[cfg(not(feature = "ffmpeg"))]
pub use unavailable::{AudioEncoder, ExportConverter};

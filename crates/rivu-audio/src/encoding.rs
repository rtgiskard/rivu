use anyhow::{Result, ensure};
#[cfg(not(feature = "ffmpeg"))]
use rivu_protocol::model::AudioExportFormat;

#[cfg(feature = "ffmpeg")]
pub use crate::ffmpeg::encode::{AudioEncoder, ExportConverter, export_encoder_available};

#[cfg(not(feature = "ffmpeg"))]
pub fn export_encoder_available(format: AudioExportFormat) -> bool {
    format == AudioExportFormat::Wav
}

/// Integer and lossy sinks accept finite samples within full scale, `-1.0..=1.0`.
pub fn validate_full_scale(samples: &[f32]) -> Result<()> {
    ensure!(
        samples.iter().all(|x| (-1.0..=1.0).contains(x)),
        "Export would clip; reduce explicit EQ output gain or choose float32 WAV"
    );
    Ok(())
}

/// Deterministic TPDF dither shared by integer WAV and FLAC export.
/// State remains continuous across blocks. Dithered, rounded codes saturate
/// at the target bounds; quantization error stays within 1.5 target LSB.
pub struct PcmDither(u64);

impl Default for PcmDither {
    fn default() -> Self {
        Self(0x7269_7675_6469_7468)
    }
}

impl PcmDither {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let value = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
    }

    /// Quantize a sample already checked by `validate_full_scale`, at 16 or 24 bits.
    pub fn quantize(&mut self, sample: f32, bits: u16) -> i32 {
        let scale = (1_u32 << (bits - 1)) as f64;
        let noise = self.uniform() - self.uniform();
        (f64::from(sample) * scale + noise)
            .round()
            .clamp(-scale, scale - 1.0) as i32
    }
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

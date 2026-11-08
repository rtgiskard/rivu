//! Native-node rules and preparation, independent of execution-mode admission.

use anyhow::{Result, bail, ensure};
use rivu_dsp::{
    denoise::{BlockDenoiser, Denoiser, NoiseProfile},
    equalizer::{config::MAX_CHANNELS, fir::FirEqualizer, iir::Equalizer},
};
use rivu_format::config::processing_graph::{EqImplementation, GraphNodeKind};

/// Format validation and causal timing without allocating processor state.
/// Profile availability and model capabilities belong to the execution mode.
pub struct NativeNodePlan<'a> {
    kind: &'a GraphNodeKind,
    rate: u32,
    channels: usize,
    bypassed: bool,
    timing: (usize, usize),
}

impl<'a> NativeNodePlan<'a> {
    pub fn new(kind: &'a GraphNodeKind, rate: u32, channels: usize) -> Result<Self> {
        let (bypassed, timing) = match kind {
            GraphNodeKind::Output | GraphNodeKind::Mix { .. } => (true, (0, 0)),
            GraphNodeKind::Equalizer {
                config,
                implementation,
            } => {
                config.validate_for_stream(rate, channels)?;
                implementation.validate()?;
                let timing = match implementation {
                    EqImplementation::Iir => (0, 0),
                    EqImplementation::LinearPhase { taps } => {
                        ensure!(
                            channels <= MAX_CHANNELS,
                            "FIR supports at most {MAX_CHANNELS} channels"
                        );
                        FirEqualizer::timing_frames(config, *taps)
                    }
                };
                (!config.enabled, timing)
            }
            GraphNodeKind::Denoise { config, .. } => {
                config.validate_for_format(rate, channels)?;
                let delay = config.lookahead_frames();
                (config.is_bypassed(), (delay, delay))
            }
            _ => bail!("Node is not a native processor"),
        };
        Ok(Self {
            kind,
            rate,
            channels,
            bypassed,
            timing,
        })
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn timing_frames(&self) -> (usize, usize) {
        self.timing
    }

    pub fn prepare(
        self,
        profile: Option<&NoiseProfile>,
        block_frames: usize,
    ) -> Result<NativeProcessor> {
        // Bypass must retain constructor errors, including profile shape/format.
        // Validate without cloning the profile or allocating a throwaway denoiser.
        if let GraphNodeKind::Denoise { config, .. } = self.kind
            && self.bypassed
        {
            config.validate_for_processor(self.rate, self.channels, profile)?;
        }
        if self.bypassed {
            return Ok(NativeProcessor::Pass);
        }
        Ok(match self.kind {
            GraphNodeKind::Equalizer {
                config,
                implementation,
            } => {
                match implementation {
                    EqImplementation::Iir => {
                        NativeProcessor::Iir(Equalizer::new(config, self.rate, self.channels)?)
                    }
                    EqImplementation::LinearPhase { taps } => NativeProcessor::Fir(
                        FirEqualizer::new(config, self.rate, self.channels, *taps)?,
                    ),
                }
            }
            GraphNodeKind::Denoise { config, .. } => {
                NativeProcessor::Denoise(Box::new(BlockDenoiser::new(
                    Denoiser::new(self.rate, self.channels, config.clone(), profile.cloned())?,
                    block_frames,
                )?))
            }
            _ => unreachable!("validated native node"),
        })
    }
}

pub enum NativeProcessor {
    Pass,
    Iir(Equalizer),
    Fir(FirEqualizer),
    Denoise(Box<BlockDenoiser>),
}

impl NativeProcessor {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    pub fn reset(&mut self) {
        match self {
            Self::Pass => {}
            Self::Iir(eq) => eq.reset(),
            Self::Fir(eq) => eq.reset(),
            Self::Denoise(denoise) => denoise.reset(),
        }
    }

    /// The caller supplies real upstream length/EOF, never padded source input.
    pub fn process(
        &mut self,
        output: &mut [f32],
        input_samples: usize,
        eof: bool,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        match self {
            Self::Pass => Ok(()),
            Self::Iir(eq) => eq.process(output).map_err(Into::into),
            Self::Fir(eq) => eq.process_interleaved(output),
            Self::Denoise(denoise) => denoise.process(output, input_samples, eof, cancelled),
        }
    }
}

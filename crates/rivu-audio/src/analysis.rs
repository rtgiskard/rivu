//! Audio analysis publications and coalesced display demand.

mod runtime;

#[cfg(not(feature = "test-support"))]
pub(crate) use runtime::AnalysisWorker;
pub(crate) use runtime::{AnalysisAudioIdentity, AnalysisProducer};
pub use runtime::{
    AnalysisControl, AnalysisFrame, AnalysisSettings, DENSITY_SIDE, DensitySnapshot, DisplayDemand,
    PcmSnapshot, SpatialBin, SpectrumSnapshot, VisualizerDemand, VisualizerFrame,
};
#[cfg(feature = "test-support")]
pub use runtime::{AnalysisWorker, TapFrame};

//! Audio analysis publications and coalesced display demand.

mod runtime;

pub(crate) use runtime::AnalysisProducer;
#[cfg(not(feature = "test-support"))]
pub(crate) use runtime::AnalysisWorker;
pub use runtime::{
    AnalysisControl, AnalysisFrame, AnalysisSettings, DENSITY_SIDE, DensitySnapshot, DisplayDemand,
    SpatialBin, VisualizerDemand, VisualizerFrame,
};
#[cfg(feature = "test-support")]
pub use runtime::{AnalysisWorker, TapFrame};

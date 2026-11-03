//! Audio analysis production and coalesced display control.

mod runtime;

pub use runtime::AnalysisControl;
#[cfg(not(feature = "test-support"))]
pub(crate) use runtime::AnalysisWorker;
pub(crate) use runtime::{AnalysisAudioIdentity, AnalysisProducer};
#[cfg(feature = "test-support")]
pub use runtime::{AnalysisWorker, TapFrame};

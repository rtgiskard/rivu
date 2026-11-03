use super::super::{DenoiseField, NeuralField};
use rivu_format::config::processing_graph::{EqImplementation, GraphNode, GraphNodeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in super::super) struct NodeField {
    pub(in super::super) node: u64,
    pub(in super::super) field: GraphField,
    kind: NodeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum NodeKind {
    Source,
    Equalizer,
    Denoise,
    Speech,
    Separate,
    Mix,
    Output,
}

impl From<&GraphNodeKind> for NodeKind {
    fn from(kind: &GraphNodeKind) -> Self {
        match kind {
            GraphNodeKind::Source => Self::Source,
            GraphNodeKind::Equalizer { .. } => Self::Equalizer,
            GraphNodeKind::Denoise { .. } => Self::Denoise,
            GraphNodeKind::Speech { .. } => Self::Speech,
            GraphNodeKind::Separate { .. } => Self::Separate,
            GraphNodeKind::Mix { .. } => Self::Mix,
            GraphNodeKind::Output => Self::Output,
        }
    }
}

impl NodeField {
    pub(in super::super) fn new(node: &GraphNode, field: GraphField) -> Self {
        Self {
            node: node.id,
            kind: NodeKind::from(&node.kind),
            field,
        }
    }

    pub(super) fn matches(self, node: &GraphNode) -> bool {
        self.node == node.id && self.kind == NodeKind::from(&node.kind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in super::super) enum GraphField {
    Implementation,
    Taps,
    Denoise(DenoiseField),
    Neural(NeuralField),
    ProfileStart,
    ProfileEnd,
    MixGain(usize),
    MixMuted(usize),
    MixSolo(usize),
}

impl GraphField {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Implementation => "processing-fields-implementation",
            Self::Taps => "processing-graph-taps",
            Self::ProfileStart => "processing-profile-start",
            Self::ProfileEnd => "processing-profile-end",
            Self::MixGain(_) => "processing-graph-input-gain",
            Self::MixMuted(_) => "processing-graph-input-muted",
            Self::MixSolo(_) => "processing-graph-input-solo",
            Self::Denoise(field) => field.label_id(),
            Self::Neural(field) => field.label_id(),
        }
    }

    pub(super) fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Implementation => &[
                ("iir", "processing-graph-iir"),
                ("linear_phase", "processing-graph-fir"),
            ],
            Self::Denoise(field) => field.choices(),
            Self::Neural(field) => field.choices(),
            _ => &[],
        }
    }

    pub(super) fn is_boolean(self) -> bool {
        match self {
            Self::Denoise(field) => field.input_kind() == super::super::FieldInputKind::Boolean,
            Self::Neural(field) => field.input_kind() == super::super::FieldInputKind::Boolean,
            Self::MixMuted(_) | Self::MixSolo(_) => true,
            _ => false,
        }
    }

    pub(super) fn numeric_step(self) -> Option<f64> {
        match self {
            Self::Denoise(field) => field.numeric_step(),
            Self::Neural(field) => field.numeric_step(),
            Self::Taps => Some(2.),
            Self::ProfileStart | Self::ProfileEnd | Self::MixGain(_) => Some(0.1),
            _ => None,
        }
    }

    pub(super) fn is_timestamp(self) -> bool {
        matches!(self, Self::ProfileStart | Self::ProfileEnd)
    }

    pub(super) fn is_path(self) -> bool {
        self.path_placeholder().is_some()
    }

    pub(in super::super) fn path_placeholder(self) -> Option<&'static str> {
        match self {
            Self::Neural(field) => field.path_placeholder(),
            _ => None,
        }
    }

    pub(in super::super) fn path_help(self) -> Option<&'static str> {
        match self {
            Self::Neural(field) => field.path_help(),
            _ => None,
        }
    }

    pub(super) fn inactive(self, kind: &GraphNodeKind) -> bool {
        use NeuralField as N;
        match kind {
            GraphNodeKind::Speech { .. } => matches!(
                self,
                Self::Neural(
                    N::SeparationInterpreter | N::SeparationManifest | N::SeparationOverlap
                )
            ),
            GraphNodeKind::Separate { .. } => matches!(
                self,
                Self::Neural(
                    N::SpeechInterpreter
                        | N::SpeechManifest
                        | N::SpeechWetMix
                        | N::SpeechAttenuationLimitDb
                        | N::SpeechPostFilter
                )
            ),
            _ => false,
        }
    }

    pub(super) fn group(self, kind: &GraphNodeKind) -> usize {
        if !matches!(
            kind,
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. }
        ) {
            0
        } else if self.is_path() || self.inactive(kind) {
            2
        } else if matches!(
            self,
            Self::Neural(NeuralField::Device | NeuralField::Threads)
        ) {
            1
        } else {
            0
        }
    }

    pub(super) fn apply(self, kind: &mut GraphNodeKind, text: &str) -> Result<(), String> {
        let text = if self.is_path() { text } else { text.trim() };
        match (self, kind) {
            (Self::Implementation, GraphNodeKind::Equalizer { implementation, .. }) => {
                *implementation = match text {
                    "iir" => EqImplementation::Iir,
                    "linear_phase" => EqImplementation::LinearPhase {
                        taps: match implementation {
                            EqImplementation::LinearPhase { taps } => *taps,
                            EqImplementation::Iir => 2049,
                        },
                    },
                    _ => return Err("Unknown equalizer implementation".into()),
                };
            }
            (Self::Taps, GraphNodeKind::Equalizer { implementation, .. }) => {
                let taps = text.parse::<usize>().map_err(|error| error.to_string())?;
                let EqImplementation::LinearPhase { taps: current } = implementation else {
                    return Err(
                        "Select linear-phase FIR and apply all to change its tap count".into(),
                    );
                };
                *current = taps;
            }
            (Self::Denoise(field), GraphNodeKind::Denoise { config, .. }) => {
                field.apply(config, text)?
            }
            (
                Self::Neural(field),
                GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config },
            ) => field.apply(config, text)?,
            (
                Self::ProfileStart,
                GraphNodeKind::Denoise {
                    profile: Some(profile),
                    ..
                },
            ) => profile.start_seconds = finite_f64(text)?,
            (
                Self::ProfileEnd,
                GraphNodeKind::Denoise {
                    profile: Some(profile),
                    ..
                },
            ) => profile.end_seconds = finite_f64(text)?,
            (Self::MixGain(index), GraphNodeKind::Mix { gains_db, .. }) => {
                *gains_db.get_mut(index).ok_or("Node field changed")? = finite_f32(text)?;
            }
            (Self::MixMuted(index), GraphNodeKind::Mix { muted, .. }) => {
                *muted.get_mut(index).ok_or("Node field changed")? =
                    text.parse::<bool>().map_err(|error| error.to_string())?;
            }
            (Self::MixSolo(index), GraphNodeKind::Mix { solo, .. }) => {
                *solo.get_mut(index).ok_or("Node field changed")? =
                    text.parse::<bool>().map_err(|error| error.to_string())?;
            }
            _ => return Err("Node field changed".into()),
        }
        Ok(())
    }

    pub(in crate::gui::audio_processing) fn text(self, kind: &GraphNodeKind) -> String {
        match (self, kind) {
            (Self::Implementation, GraphNodeKind::Equalizer { implementation, .. }) => {
                match implementation {
                    EqImplementation::Iir => "iir",
                    EqImplementation::LinearPhase { .. } => "linear_phase",
                }
                .to_owned()
            }
            (Self::Taps, GraphNodeKind::Equalizer { implementation, .. }) => match implementation {
                EqImplementation::Iir => 2049,
                EqImplementation::LinearPhase { taps } => *taps,
            }
            .to_string(),
            (Self::Denoise(field), GraphNodeKind::Denoise { config, .. }) => field.text(config),
            (
                Self::Neural(field),
                GraphNodeKind::Speech { config } | GraphNodeKind::Separate { config },
            ) => field.text(config),
            (
                Self::ProfileStart,
                GraphNodeKind::Denoise {
                    profile: Some(profile),
                    ..
                },
            ) => profile.start_seconds.to_string(),
            (
                Self::ProfileEnd,
                GraphNodeKind::Denoise {
                    profile: Some(profile),
                    ..
                },
            ) => profile.end_seconds.to_string(),
            (Self::MixGain(index), GraphNodeKind::Mix { gains_db, .. }) => {
                gains_db[index].to_string()
            }
            (Self::MixMuted(index), GraphNodeKind::Mix { muted, .. }) => muted[index].to_string(),
            (Self::MixSolo(index), GraphNodeKind::Mix { solo, .. }) => solo[index].to_string(),
            _ => unreachable!("field descriptor belongs to its node kind"),
        }
    }
}

fn finite_f64(text: &str) -> Result<f64, String> {
    let value = text.parse::<f64>().map_err(|error| error.to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("Enter a finite number".into())
    }
}

fn finite_f32(text: &str) -> Result<f32, String> {
    let value = text.parse::<f32>().map_err(|error| error.to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("Enter a finite number".into())
    }
}

/// Explicit presentation order, independent of serialization layout. Enumerating
/// controls does not format values or allocate a temporary editing tree.
pub(in crate::gui::audio_processing) fn node_fields(
    kind: &GraphNodeKind,
) -> impl Iterator<Item = GraphField> {
    use DenoiseField as D;
    use GraphField::{Denoise, Neural};
    use NeuralField as N;
    let (fixed, profile, gains, muted, solo): (&[GraphField], bool, usize, usize, usize) =
        match kind {
            GraphNodeKind::Equalizer { .. } => (
                &[GraphField::Implementation, GraphField::Taps],
                false,
                0,
                0,
                0,
            ),
            GraphNodeKind::Denoise { profile, .. } => (
                &[
                    Denoise(D::AttackMs),
                    Denoise(D::FftSize),
                    Denoise(D::HumEnabled),
                    Denoise(D::HumFundamentalHz),
                    Denoise(D::HumHarmonics),
                    Denoise(D::HumQ),
                    Denoise(D::ImpulseEnabled),
                    Denoise(D::ImpulseFloorDbfs),
                    Denoise(D::ImpulseMaxWidth),
                    Denoise(D::ImpulseThreshold),
                    Denoise(D::ImpulseTransientProtection),
                    Denoise(D::Mode),
                    Denoise(D::SpectralGainFloorDb),
                    Denoise(D::ReleaseMs),
                    Denoise(D::SmoothingBins),
                    Denoise(D::StationaryNoiseDbfs),
                    Denoise(D::Strength),
                    Denoise(D::TransientProtection),
                    Denoise(D::Wet),
                ],
                profile.is_some(),
                0,
                0,
                0,
            ),
            GraphNodeKind::Speech { .. } | GraphNodeKind::Separate { .. } => (
                &[
                    Neural(N::Device),
                    Neural(N::SeparationInterpreter),
                    Neural(N::SeparationManifest),
                    Neural(N::SeparationOverlap),
                    Neural(N::SpeechAttenuationLimitDb),
                    Neural(N::SpeechInterpreter),
                    Neural(N::SpeechManifest),
                    Neural(N::SpeechPostFilter),
                    Neural(N::SpeechWetMix),
                    Neural(N::Threads),
                    Neural(N::Worker),
                ],
                false,
                0,
                0,
                0,
            ),
            GraphNodeKind::Mix {
                gains_db,
                muted,
                solo,
            } => (&[], false, gains_db.len(), muted.len(), solo.len()),
            GraphNodeKind::Source | GraphNodeKind::Output => (&[], false, 0, 0, 0),
        };
    fixed
        .iter()
        .copied()
        .chain(profile.then_some(GraphField::ProfileEnd))
        .chain(profile.then_some(GraphField::ProfileStart))
        .chain((0..gains).map(GraphField::MixGain))
        .chain((0..muted).map(GraphField::MixMuted))
        .chain((0..solo).map(GraphField::MixSolo))
}

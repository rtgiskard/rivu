//! Speaker positions in decoded/interleaved order. No implicit downmix is performed.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Channel {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    RearLeft,
    RearRight,
    FrontLeftCenter,
    FrontRightCenter,
    RearCenter,
    SideLeft,
    SideRight,
    TopCenter,
    TopFrontLeft,
    TopFrontCenter,
    TopFrontRight,
    TopRearLeft,
    TopRearCenter,
    TopRearRight,
}

impl Channel {
    // WAVE speaker-mask bits and FFmpeg's first 18 AVChannel identifiers share
    // these positions. Keep the mapping explicit rather than transmuting FFI.
    pub(super) fn from_standard_index(index: u32) -> Option<Self> {
        Some(match index {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::Lfe,
            4 => Self::RearLeft,
            5 => Self::RearRight,
            6 => Self::FrontLeftCenter,
            7 => Self::FrontRightCenter,
            8 => Self::RearCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            11 => Self::TopCenter,
            12 => Self::TopFrontLeft,
            13 => Self::TopFrontCenter,
            14 => Self::TopFrontRight,
            15 => Self::TopRearLeft,
            16 => Self::TopRearCenter,
            17 => Self::TopRearRight,
            _ => return None,
        })
    }
}

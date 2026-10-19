use super::Track;
use serde::{Deserialize, Serialize};

/// Same cached whole-file fingerprint, not a newly verified content assertion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub fingerprint: String,
    pub total_members: usize,
    pub missing_members: usize,
    pub present_members: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DuplicateMember {
    pub track: Track,
    pub file_size: u64,
    pub modified_ns: i64,
    pub has_overrides: bool,
    pub playlist_references: usize,
    pub queue_references: usize,
    pub independent: bool,
}

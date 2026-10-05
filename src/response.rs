use crate::projection::ClientSnapshot;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub state: ClientSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    pub error: Option<String>,
    pub revision: u64,
}

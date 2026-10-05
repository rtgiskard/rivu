use super::*;
use anyhow::bail;
use crossbeam_channel::{Receiver, select};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[path = "catalog.rs"]
mod catalog;
#[path = "command.rs"]
mod command;
#[path = "persistence.rs"]
mod persistence;
#[path = "playback.rs"]
mod playback;
#[path = "playlists.rs"]
mod playlists;
#[path = "queue.rs"]
mod queue;
#[path = "runtime.rs"]
mod runtime;

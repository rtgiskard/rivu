use super::*;
use anyhow::bail;
use crossbeam_channel::{Receiver, select};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[path = "core_impl/catalog.rs"]
mod catalog;
#[path = "core_impl/command.rs"]
mod command;
#[path = "core_impl/persistence.rs"]
mod persistence;
#[path = "core_impl/playback.rs"]
mod playback;
#[path = "core_impl/playlists.rs"]
mod playlists;
#[path = "core_impl/queue.rs"]
mod queue;
#[path = "core_impl/runtime.rs"]
mod runtime;

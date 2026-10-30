mod album_artwork;
mod album_browser;
mod animation_frames;
mod appearance;
mod artwork;
mod audio_processing;
mod components;
mod coordination;
mod duplicates;
mod feature;
mod history;
mod interaction;
mod layout;
mod library;
mod list_interaction;
mod metadata;
mod paged;
mod playback;
mod playlists;
mod query;
mod queue;
mod scrolling;
pub(crate) mod session;
mod settings;
mod shell;
mod visualization;
mod window;
mod workspace;

use list_interaction::{ContentDrag, ContentMenuRequest, DragDestination, TrackSource};
pub(crate) use window::WindowView;
use window::{BrowserFocus, ListFocus};

use anyhow::Result;
use ashpd::desktop::file_chooser::SelectedFiles;
use gpui::App;
use rivu_ui::input::Input;
use std::path::PathBuf;
use url::Url;

pub(crate) fn init(cx: &mut App) -> Result<()> {
    #[cfg(feature = "embed-nerd-font")]
    appearance::fonts::register_symbols(cx)
        .map_err(|error| error.context("Could not load embedded icon font"))?;
    Input::init(cx);
    Ok(())
}

fn chooser_path(files: &SelectedFiles) -> Option<PathBuf> {
    files
        .uris()
        .first()
        .and_then(|uri| Url::parse(uri.as_str()).ok())
        .and_then(|uri| uri.to_file_path().ok())
}

fn chooser_result<T>(
    result: std::result::Result<T, ashpd::Error>,
) -> std::result::Result<Option<T>, ashpd::Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn format_time(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

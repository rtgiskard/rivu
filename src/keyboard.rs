use crate::model::{Command, PlaybackState, PlaybackStatus, RepeatMode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
    pub function: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Context {
    Gui,
    GuiLibrary,
    GuiQueue,
    GuiPlaylistTracks,
    GuiHistory,
    GuiPlaylists,
    GuiAlbums,
    TuiQueue,
    TuiSearchQuery,
    TuiSearchResults,
    TuiTree,
    TuiPlaylists,
    TuiPlaylistTracks,
    TuiHelp,
    GuiHelp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Help,
    Details,
    Retry,
    Cancel,
    Close,
    Search,
    Library,
    Playlists,
    Toggle,
    Next,
    Previous,
    VolumeDown,
    VolumeUp,
    Repeat,
    Shuffle,
    SeekBackward,
    SeekForward,
    SeekStart,
    SeekEnd,
    Up,
    Down,
    First,
    Last,
    PageUp,
    PageDown,
    Activate,
    Remove,
    Parent,
    Expand,
    Enqueue,
    Traverse,
    SelectAll,
    ExtendUp,
    ExtendDown,
    AddExtendUp,
    AddExtendDown,
    Backspace,
}

pub(crate) struct Binding {
    pub label: &'static str,
    pub description: &'static str,
    pub action: Action,
    key: &'static str,
    modifiers: u8,
    contexts: u16,
}

const SHIFT: u8 = 1;
const CONTROL: u8 = 2;
const PLATFORM: u8 = 4;
const GUI: u16 = (1 << 7) - 1;
const GUI_LIST: u16 = GUI & !(1 << Context::Gui as u16);
const TUI: u16 = ((1 << 13) - 1) & !GUI;
const QUERY: u16 = 1 << Context::TuiSearchQuery as u16;
const HELP: u16 = (1 << Context::TuiHelp as u16) | (1 << Context::GuiHelp as u16);
const LIST: u16 = (1 << Context::GuiLibrary as u16)
    | (1 << Context::GuiQueue as u16)
    | (1 << Context::GuiPlaylistTracks as u16)
    | (TUI & !QUERY);
const PLAYBACK: u16 = GUI | (TUI & !QUERY);
const QUEUE: u16 = (1 << Context::GuiQueue as u16) | (1 << Context::TuiQueue as u16);
const TRACKS: u16 =
    (1 << Context::GuiPlaylistTracks as u16) | (1 << Context::TuiPlaylistTracks as u16);
const TREE: u16 = (1 << Context::GuiLibrary as u16) | (1 << Context::TuiTree as u16);
const SEARCH: u16 = QUERY | (1 << Context::TuiSearchResults as u16);
const MULTI: u16 = (1 << Context::GuiQueue as u16)
    | (1 << Context::GuiPlaylistTracks as u16)
    | (1 << Context::GuiHistory as u16);

// Key, displayed label, modifiers, scope, action and localized description are
// declared together. Dispatch and help filter this same bounded static table.
macro_rules! binding {
    ($key:literal, $label:literal, $mods:expr, $contexts:expr, $action:ident, $description:literal) => {
        Binding {
            key: $key,
            label: $label,
            modifiers: $mods,
            contexts: $contexts,
            action: Action::$action,
            description: $description,
        }
    };
}
static BINDINGS: &[Binding] = &[
    binding!("f1", "F1", 0, GUI | TUI | HELP, Help, "keyboard-help"),
    binding!("i", "i", 0, TUI & !QUERY, Details, "keyboard-details"),
    binding!(
        "f5",
        "F5",
        0,
        GUI_LIST & !(1 << Context::GuiHistory as u16) | TUI,
        Retry,
        "keyboard-retry"
    ),
    binding!(
        "escape",
        "Escape",
        0,
        GUI | TUI | HELP,
        Cancel,
        "keyboard-cancel"
    ),
    binding!("q", "q", 0, GUI | (TUI & !SEARCH), Close, "keyboard-close"),
    binding!("f", "Ctrl+F", CONTROL, GUI_LIST, Search, "keyboard-search"),
    binding!("f", "Cmd+F", PLATFORM, GUI_LIST, Search, "keyboard-search"),
    binding!(
        "/",
        "/",
        0,
        TUI & !SEARCH,
        Search,
        "keyboard-library-search"
    ),
    binding!(
        "t",
        "t",
        0,
        GUI | (TUI & !SEARCH),
        Library,
        "keyboard-library"
    ),
    binding!(
        "p",
        "P",
        SHIFT,
        TUI & !SEARCH,
        Playlists,
        "keyboard-playlists"
    ),
    binding!("space", "Space", 0, PLAYBACK, Toggle, "keyboard-toggle"),
    binding!("n", "n", 0, PLAYBACK, Next, "keyboard-next"),
    binding!("p", "p", 0, PLAYBACK, Previous, "keyboard-previous"),
    binding!("[", "[", 0, PLAYBACK, VolumeDown, "keyboard-volume-down"),
    binding!("]", "]", 0, PLAYBACK, VolumeUp, "keyboard-volume-up"),
    binding!("r", "r", 0, PLAYBACK, Repeat, "keyboard-repeat"),
    binding!("s", "s", 0, PLAYBACK, Shuffle, "keyboard-shuffle"),
    binding!(
        "left",
        "Ctrl+Left",
        CONTROL,
        PLAYBACK,
        SeekBackward,
        "keyboard-seek-backward"
    ),
    binding!(
        "right",
        "Ctrl+Right",
        CONTROL,
        PLAYBACK,
        SeekForward,
        "keyboard-seek-forward"
    ),
    binding!(
        "home",
        "Ctrl+Home",
        CONTROL,
        PLAYBACK,
        SeekStart,
        "keyboard-seek-start"
    ),
    binding!(
        "end",
        "Ctrl+End",
        CONTROL,
        PLAYBACK,
        SeekEnd,
        "keyboard-seek-end"
    ),
    binding!("up", "Up", 0, LIST | HELP, Up, "keyboard-up"),
    binding!("down", "Down", 0, LIST | HELP, Down, "keyboard-down"),
    binding!("home", "Home", 0, LIST | HELP, First, "keyboard-first"),
    binding!("end", "End", 0, LIST | HELP, Last, "keyboard-last"),
    binding!(
        "pageup",
        "PageUp",
        0,
        GUI | (TUI & !QUERY) | HELP,
        PageUp,
        "keyboard-page-up"
    ),
    binding!(
        "pagedown",
        "PageDown",
        0,
        GUI | (TUI & !QUERY) | HELP,
        PageDown,
        "keyboard-page-down"
    ),
    binding!(
        "enter",
        "Enter",
        0,
        LIST | QUERY,
        Activate,
        "keyboard-activate"
    ),
    binding!(
        "delete",
        "Delete",
        0,
        QUEUE | TRACKS,
        Remove,
        "keyboard-remove"
    ),
    binding!(
        "left",
        "Left",
        0,
        TREE | (1 << Context::TuiPlaylistTracks as u16),
        Parent,
        "keyboard-parent"
    ),
    binding!(
        "backspace",
        "Backspace",
        0,
        1 << Context::TuiTree as u16,
        Parent,
        "keyboard-parent"
    ),
    binding!(
        "right",
        "Right",
        0,
        TREE | (1 << Context::TuiPlaylists as u16),
        Expand,
        "keyboard-expand"
    ),
    binding!(
        "a",
        "a",
        0,
        1 << Context::TuiPlaylistTracks as u16,
        Enqueue,
        "keyboard-enqueue"
    ),
    binding!("tab", "Tab", 0, GUI | SEARCH, Traverse, "keyboard-traverse"),
    binding!(
        "tab",
        "Shift+Tab",
        SHIFT,
        GUI | SEARCH,
        Traverse,
        "keyboard-traverse"
    ),
    binding!(
        "backspace",
        "Backspace",
        0,
        QUERY,
        Backspace,
        "keyboard-backspace"
    ),
    binding!(
        "a",
        "Ctrl+A",
        CONTROL,
        MULTI,
        SelectAll,
        "keyboard-select-all"
    ),
    binding!(
        "a",
        "Cmd+A",
        PLATFORM,
        MULTI,
        SelectAll,
        "keyboard-select-all"
    ),
    binding!(
        "up",
        "Shift+Up",
        SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        ExtendUp,
        "keyboard-extend-up"
    ),
    binding!(
        "down",
        "Shift+Down",
        SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        ExtendDown,
        "keyboard-extend-down"
    ),
    binding!(
        "up",
        "Ctrl+Shift+Up",
        CONTROL | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendUp,
        "keyboard-add-extend-up"
    ),
    binding!(
        "down",
        "Ctrl+Shift+Down",
        CONTROL | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendDown,
        "keyboard-add-extend-down"
    ),
    binding!(
        "up",
        "Cmd+Shift+Up",
        PLATFORM | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendUp,
        "keyboard-add-extend-up"
    ),
    binding!(
        "down",
        "Cmd+Shift+Down",
        PLATFORM | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendDown,
        "keyboard-add-extend-down"
    ),
];

pub(crate) fn bindings(context: Context) -> impl Iterator<Item = &'static Binding> {
    BINDINGS
        .iter()
        .filter(move |binding| binding.contexts & (1 << context as u16) != 0)
}

pub(crate) fn resolve(key: &str, mut modifiers: Modifiers, context: Context) -> Option<Action> {
    if modifiers.alt || modifiers.function {
        return None;
    }
    // Crossterm reports uppercase characters, whereas GPUI reports a lowercase
    // key plus Shift. Normalize ASCII letters without allocating a String.
    let mut lower = [0_u8; 1];
    let key = if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
        lower[0] = key.as_bytes()[0].to_ascii_lowercase();
        modifiers.shift = true;
        std::str::from_utf8(&lower).expect("ASCII letter")
    } else if key == " " {
        "space"
    } else {
        key
    };
    let bits = (u8::from(modifiers.shift) * SHIFT)
        | (u8::from(modifiers.control) * CONTROL)
        | (u8::from(modifiers.platform) * PLATFORM);
    bindings(context)
        .find(|binding| binding.key == key && binding.modifiers == bits)
        .map(|binding| binding.action)
}

pub(crate) fn playback_command(action: Action, playback: &PlaybackState) -> Option<Command> {
    let seek = |seconds: f64| {
        (playback.status != PlaybackStatus::Stopped).then_some(Command::Seek { seconds })
    };
    match action {
        Action::Toggle => Some(Command::Toggle),
        Action::Next => Some(Command::Next),
        Action::Previous => Some(Command::Previous),
        Action::SeekBackward => seek((playback.position - 5.0).max(0.0)),
        Action::SeekForward => seek((playback.position + 5.0).max(0.0)),
        Action::SeekStart => seek(0.0),
        Action::SeekEnd => playback
            .duration
            .filter(|duration| duration.is_finite())
            .and_then(seek),
        Action::Repeat => Some(Command::Repeat {
            mode: match playback.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            },
        }),
        Action::Shuffle => Some(Command::Shuffle {
            enabled: !playback.shuffle,
        }),
        Action::VolumeUp => Some(Command::Volume {
            value: (playback.volume + 0.05).min(1.0),
        }),
        Action::VolumeDown => Some(Command::Volume {
            value: (playback.volume - 0.05).max(0.0),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_require_the_declared_modifier_combination() {
        for context in [Context::GuiQueue, Context::TuiQueue] {
            assert_eq!(resolve("left", Modifiers::default(), context), None);
            assert_eq!(
                resolve(
                    "left",
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    context
                ),
                Some(Action::SeekBackward)
            );
            for modifiers in [
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                Modifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
                Modifiers {
                    platform: true,
                    ..Default::default()
                },
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                Modifiers {
                    function: true,
                    ..Default::default()
                },
            ] {
                assert_eq!(resolve("q", modifiers, context), None);
                assert_eq!(resolve("space", modifiers, context), None);
            }
            for key in ["d", "?", "R"] {
                assert_eq!(resolve(key, Modifiers::default(), context), None);
            }
        }
        assert_eq!(
            resolve("P", Modifiers::default(), Context::TuiQueue),
            Some(Action::Playlists)
        );
        assert_eq!(
            resolve("p", Modifiers::default(), Context::TuiQueue),
            Some(Action::Previous)
        );
        assert_eq!(
            resolve(
                "a",
                Modifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
                Context::GuiQueue
            ),
            None
        );
    }

    #[test]
    fn playback_seek_respects_status_and_boundaries() {
        let mut state = PlaybackState {
            position: 2.0,
            ..Default::default()
        };
        assert!(playback_command(Action::SeekBackward, &state).is_none());
        state.status = PlaybackStatus::Playing;
        assert!(
            matches!(playback_command(Action::SeekBackward, &state), Some(Command::Seek { seconds }) if seconds == 0.0)
        );
        state.position = 12.0;
        assert!(
            matches!(playback_command(Action::SeekForward, &state), Some(Command::Seek { seconds }) if seconds == 17.0)
        );
        state.duration = Some(f64::INFINITY);
        assert!(playback_command(Action::SeekEnd, &state).is_none());
        state.duration = Some(42.0);
        assert!(
            matches!(playback_command(Action::SeekEnd, &state), Some(Command::Seek { seconds }) if seconds == 42.0)
        );
        assert!(playback_command(Action::First, &state).is_none());
    }

    #[test]
    fn playback_volume_and_repeat_cycle_are_bounded() {
        let mut state = PlaybackState {
            volume: 1.0,
            ..Default::default()
        };
        assert!(
            matches!(playback_command(Action::VolumeUp, &state), Some(Command::Volume { value }) if value == 1.0)
        );
        state.volume = 0.0;
        assert!(
            matches!(playback_command(Action::VolumeDown, &state), Some(Command::Volume { value }) if value == 0.0)
        );
        for (before, after) in [
            (RepeatMode::Off, RepeatMode::All),
            (RepeatMode::All, RepeatMode::One),
            (RepeatMode::One, RepeatMode::Off),
        ] {
            state.repeat = before;
            assert!(
                matches!(playback_command(Action::Repeat, &state), Some(Command::Repeat { mode }) if mode == after)
            );
        }
    }
}

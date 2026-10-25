use rivu_protocol::model::{Command, PlaybackState, PlaybackStatus};

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
    Favorite,
    Cancel,
    Close,
    Search,
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
const TABS: u16 = (1 << Context::TuiQueue as u16)
    | (1 << Context::TuiTree as u16)
    | (1 << Context::TuiPlaylists as u16)
    | (1 << Context::TuiPlaylistTracks as u16);
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
    binding!(
        "f1",
        "F1",
        0,
        TUI | (1 << Context::TuiHelp as u16),
        Help,
        "keyboard-help"
    ),
    binding!(
        "?",
        "?",
        0,
        (TUI | (1 << Context::TuiHelp as u16)) & !QUERY,
        Help,
        "keyboard-help"
    ),
    binding!(
        "?",
        "?",
        SHIFT,
        (TUI | (1 << Context::TuiHelp as u16)) & !QUERY,
        Help,
        "keyboard-help"
    ),
    binding!("i", "i", 0, TUI & !QUERY, Details, "keyboard-details"),
    binding!(
        "escape",
        "Escape",
        0,
        GUI | TUI | HELP,
        Cancel,
        "keyboard-cancel"
    ),
    binding!("q", "q", 0, TUI & !SEARCH, Close, "keyboard-close"),
    binding!(
        "c",
        "Ctrl+C",
        CONTROL,
        TUI | (1 << Context::TuiHelp as u16),
        Close,
        "keyboard-close"
    ),
    binding!("f", "Ctrl+F", CONTROL, GUI_LIST, Search, "keyboard-search"),
    binding!(
        "/",
        "/",
        0,
        TUI & !SEARCH,
        Search,
        "keyboard-library-search"
    ),
    binding!("space", "Space", 0, PLAYBACK, Toggle, "keyboard-toggle"),
    binding!("n", "n", 0, PLAYBACK, Next, "keyboard-next-previous"),
    binding!("p", "p", 0, PLAYBACK, Previous, "keyboard-next-previous"),
    binding!("[", "[", 0, PLAYBACK, VolumeDown, "keyboard-volume-pair"),
    binding!("]", "]", 0, PLAYBACK, VolumeUp, "keyboard-volume-pair"),
    binding!("r", "r", 0, PLAYBACK, Repeat, "keyboard-repeat"),
    binding!("s", "s", 0, PLAYBACK, Shuffle, "keyboard-shuffle"),
    binding!(
        "left",
        "Ctrl+Left",
        CONTROL,
        PLAYBACK,
        SeekBackward,
        "keyboard-seek-pair"
    ),
    binding!(
        "right",
        "Ctrl+Right",
        CONTROL,
        PLAYBACK,
        SeekForward,
        "keyboard-seek-pair"
    ),
    binding!(
        "home",
        "Ctrl+Home",
        CONTROL,
        TUI & !QUERY,
        SeekStart,
        "keyboard-seek-edges"
    ),
    binding!(
        "end",
        "Ctrl+End",
        CONTROL,
        TUI & !QUERY,
        SeekEnd,
        "keyboard-seek-edges"
    ),
    binding!("up", "Up", 0, LIST | HELP, Up, "keyboard-move-pair"),
    binding!("down", "Down", 0, LIST | HELP, Down, "keyboard-move-pair"),
    binding!("home", "Home", 0, LIST | HELP, First, "keyboard-list-edges"),
    binding!("end", "End", 0, LIST | HELP, Last, "keyboard-list-edges"),
    binding!(
        "pageup",
        "PageUp",
        0,
        GUI | (TUI & !QUERY) | HELP,
        PageUp,
        "keyboard-page-pair"
    ),
    binding!(
        "pagedown",
        "PageDown",
        0,
        GUI | (TUI & !QUERY) | HELP,
        PageDown,
        "keyboard-page-pair"
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
    binding!(
        "m",
        "m",
        0,
        TUI & !(QUERY | (1 << Context::TuiPlaylists as u16)),
        Favorite,
        "keyboard-favorite"
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
    binding!("tab", "Tab", 0, TABS, Traverse, "keyboard-tabs-pair"),
    binding!(
        "tab",
        "Shift+Tab",
        SHIFT,
        TABS,
        Traverse,
        "keyboard-tabs-pair"
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
        "up",
        "Shift+Up",
        SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        ExtendUp,
        "keyboard-extend-pair"
    ),
    binding!(
        "down",
        "Shift+Down",
        SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        ExtendDown,
        "keyboard-extend-pair"
    ),
    binding!(
        "up",
        "Ctrl+Shift+Up",
        CONTROL | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendUp,
        "keyboard-add-extend-pair"
    ),
    binding!(
        "down",
        "Ctrl+Shift+Down",
        CONTROL | SHIFT,
        1 << Context::GuiPlaylistTracks as u16,
        AddExtendDown,
        "keyboard-add-extend-pair"
    ),
];

pub(crate) fn bindings(context: Context) -> impl Iterator<Item = &'static Binding> {
    BINDINGS
        .iter()
        .filter(move |binding| binding.contexts & (1 << context as u16) != 0)
}

pub(crate) struct HelpRow {
    pub description: &'static str,
    pub action: Action,
    contexts: u16,
}

impl HelpRow {
    pub fn label(&self) -> String {
        let mut label = String::new();
        for (index, binding) in BINDINGS.iter().enumerate() {
            if binding.contexts & self.contexts == 0
                || binding.description != self.description
                || BINDINGS[..index].iter().any(|earlier| {
                    earlier.contexts & self.contexts != 0
                        && earlier.description == self.description
                        && earlier.label == binding.label
                })
            {
                continue;
            }
            if let Some((prefix, key)) = binding.label.rsplit_once('+')
                && label.starts_with(prefix)
                && label.as_bytes().get(prefix.len()) == Some(&b'+')
            {
                label.push('/');
                label.push_str(key);
            } else {
                if !label.is_empty() {
                    label.push_str(" / ");
                }
                label.push_str(binding.label);
            }
        }
        label
    }
}

pub(crate) fn help_rows(contexts: &[Context]) -> impl Iterator<Item = HelpRow> + use<> {
    let contexts = contexts
        .iter()
        .fold(0, |mask, context| mask | (1 << *context as u16));
    BINDINGS
        .iter()
        .enumerate()
        .filter_map(move |(index, binding)| {
            (binding.contexts & contexts != 0
                && !BINDINGS[..index].iter().any(|earlier| {
                    earlier.contexts & contexts != 0 && earlier.description == binding.description
                }))
            .then_some(HelpRow {
                description: binding.description,
                action: binding.action,
                contexts,
            })
        })
}

pub(crate) fn resolve(key: &str, mut modifiers: Modifiers, context: Context) -> Option<Action> {
    if modifiers.alt || modifiers.function || modifiers.platform {
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
    let bits = (u8::from(modifiers.shift) * SHIFT) | (u8::from(modifiers.control) * CONTROL);
    bindings(context)
        .find(|binding| binding.key == key && binding.modifiers == bits)
        .map(|binding| binding.action)
}

pub(crate) fn playback_command(
    action: Action,
    playback: &PlaybackState,
    queue_id: Option<u64>,
) -> Option<Command> {
    let queue_id = queue_id.filter(|_| playback.status != PlaybackStatus::Stopped);
    match action {
        Action::Toggle => Some(Command::Toggle),
        Action::Next => Some(Command::Next),
        Action::Previous => Some(Command::Previous),
        Action::SeekBackward | Action::SeekForward => {
            queue_id.map(|queue_id| Command::SeekRelative {
                queue_id,
                delta: if action == Action::SeekForward {
                    5.0
                } else {
                    -5.0
                },
            })
        }
        Action::SeekStart => queue_id.map(|queue_id| Command::SeekQueue {
            queue_id,
            seconds: 0.0,
        }),
        Action::SeekEnd => queue_id.and_then(|queue_id| {
            playback
                .duration
                .filter(|duration| duration.is_finite() && *duration >= 0.0)
                .map(|seconds| Command::SeekQueue { queue_id, seconds })
        }),
        Action::Repeat => Some(Command::CycleRepeat),
        Action::Shuffle => Some(Command::ToggleShuffle),
        Action::VolumeUp => Some(Command::AdjustVolume { delta: 0.05 }),
        Action::VolumeDown => Some(Command::AdjustVolume { delta: -0.05 }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn super_is_not_a_control_alias() {
        for (key, context, shift, action) in [
            ("f", Context::GuiLibrary, false, Action::Search),
            ("a", Context::GuiQueue, false, Action::SelectAll),
            (
                "down",
                Context::GuiPlaylistTracks,
                true,
                Action::AddExtendDown,
            ),
        ] {
            assert_eq!(
                resolve(
                    key,
                    Modifiers {
                        control: true,
                        shift,
                        ..Default::default()
                    },
                    context
                ),
                Some(action)
            );
            for control in [false, true] {
                assert_eq!(
                    resolve(
                        key,
                        Modifiers {
                            control,
                            shift,
                            platform: true,
                            ..Default::default()
                        },
                        context
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn terminal_only_actions_do_not_leak_into_desktop_contexts() {
        for (key, control, action) in [
            ("q", false, Action::Close),
            ("home", true, Action::SeekStart),
            ("end", true, Action::SeekEnd),
        ] {
            let modifiers = Modifiers {
                control,
                ..Default::default()
            };
            for context in [
                Context::Gui,
                Context::GuiLibrary,
                Context::GuiQueue,
                Context::GuiPlaylists,
                Context::GuiPlaylistTracks,
                Context::GuiAlbums,
                Context::GuiHistory,
                Context::GuiHelp,
            ] {
                assert_eq!(resolve(key, modifiers, context), None);
            }
            assert_eq!(resolve(key, modifiers, Context::TuiQueue), Some(action));
        }
    }

    #[test]
    fn help_shortcuts_belong_to_terminal_contexts_only() {
        for context in [
            Context::Gui,
            Context::GuiQueue,
            Context::GuiLibrary,
            Context::GuiHelp,
        ] {
            assert_eq!(resolve("f1", Modifiers::default(), context), None);
            for shift in [false, true] {
                assert_eq!(
                    resolve(
                        "?",
                        Modifiers {
                            shift,
                            ..Default::default()
                        },
                        context
                    ),
                    None
                );
            }
        }
        for context in [Context::TuiQueue, Context::TuiHelp] {
            assert_eq!(
                resolve("f1", Modifiers::default(), context),
                Some(Action::Help)
            );
            for shift in [false, true] {
                assert_eq!(
                    resolve(
                        "?",
                        Modifiers {
                            shift,
                            ..Default::default()
                        },
                        context
                    ),
                    Some(Action::Help)
                );
            }
            assert_eq!(
                resolve(
                    "?",
                    Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    context
                ),
                None
            );
        }
        assert_eq!(
            resolve("?", Modifiers::default(), Context::TuiSearchQuery),
            None
        );
    }

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
            for key in ["d", "R"] {
                assert_eq!(resolve(key, Modifiers::default(), context), None);
            }
        }
        assert_eq!(resolve("P", Modifiers::default(), Context::TuiQueue), None);
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
    fn terminal_tabs_traverse_pages_without_changing_search_or_gui_bindings() {
        for context in [
            Context::TuiQueue,
            Context::TuiTree,
            Context::TuiPlaylists,
            Context::TuiPlaylistTracks,
            Context::TuiSearchQuery,
            Context::TuiSearchResults,
            Context::Gui,
            Context::GuiLibrary,
            Context::GuiQueue,
            Context::GuiPlaylistTracks,
            Context::GuiHistory,
            Context::GuiPlaylists,
            Context::GuiAlbums,
        ] {
            for shift in [false, true] {
                assert_eq!(
                    resolve(
                        "tab",
                        Modifiers {
                            shift,
                            ..Default::default()
                        },
                        context
                    ),
                    Some(Action::Traverse)
                );
            }
        }
        for context in [Context::TuiHelp, Context::GuiHelp] {
            for shift in [false, true] {
                assert_eq!(
                    resolve(
                        "tab",
                        Modifiers {
                            shift,
                            ..Default::default()
                        },
                        context
                    ),
                    None
                );
            }
        }
        for context in [
            Context::TuiQueue,
            Context::TuiTree,
            Context::TuiPlaylists,
            Context::TuiPlaylistTracks,
            Context::TuiSearchQuery,
            Context::TuiSearchResults,
            Context::TuiHelp,
        ] {
            for key in ["t", "P"] {
                assert_eq!(resolve(key, Modifiers::default(), context), None);
            }
            assert_eq!(
                resolve(
                    "p",
                    Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                    context
                ),
                None
            );
        }
    }

    #[test]
    fn playback_seek_requires_active_occurrence_and_keeps_relative_intent() {
        let mut state = PlaybackState {
            position: 2.0,
            ..Default::default()
        };
        for action in [
            Action::SeekBackward,
            Action::SeekForward,
            Action::SeekStart,
            Action::SeekEnd,
        ] {
            assert!(playback_command(action, &state, Some(7)).is_none());
            state.status = PlaybackStatus::Playing;
            assert!(playback_command(action, &state, None).is_none());
            state.status = PlaybackStatus::Stopped;
        }
        for status in [PlaybackStatus::Playing, PlaybackStatus::Paused] {
            state.status = status;
            for (action, expected) in [(Action::SeekBackward, -5.0), (Action::SeekForward, 5.0)] {
                assert!(matches!(
                    playback_command(action, &state, Some(7)),
                    Some(Command::SeekRelative { queue_id: 7, delta }) if delta == expected
                ));
            }
            assert!(matches!(
                playback_command(Action::SeekStart, &state, Some(7)),
                Some(Command::SeekQueue {
                    queue_id: 7,
                    seconds: 0.0
                })
            ));
            for duration in [None, Some(f64::INFINITY), Some(f64::NAN), Some(-1.0)] {
                state.duration = duration;
                assert!(playback_command(Action::SeekEnd, &state, Some(7)).is_none());
            }
            state.duration = Some(42.0);
            assert!(matches!(
                playback_command(Action::SeekEnd, &state, Some(7)),
                Some(Command::SeekQueue {
                    queue_id: 7,
                    seconds: 42.0
                })
            ));
        }
        assert!(playback_command(Action::First, &state, Some(7)).is_none());
    }
}

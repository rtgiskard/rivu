use crate::{
    app::AppHandle,
    core::{CoreHandle, RevisionInterest},
    projection::TraySnapshot,
};
use futures::{FutureExt, StreamExt, channel::oneshot, future::Shared};
use parking_lot::RwLock;
use rivu_presentation::i18n::Localizer;
use rivu_presentation::track::{playback_title, playback_title_changed};
use rivu_protocol::model::{Command, PlaybackStatus, RepeatMode};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use zbus::{
    AsyncDrop, Connection, ObjectPath, OwnedVariant, Proxy, Type, Variant, connection::Builder,
    fdo, wire::StructureBuilder,
};

const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const SNI_PATH: &str = "/StatusNotifierItem";
const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";
const MENU_PATH: &str = "/Menu";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(1);
type IconPixmap = (i32, i32, Vec<u8>);
type Cancellation = Shared<oneshot::Receiver<()>>;

fn sni_status() -> &'static str {
    "Active"
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TrayMenuState {
    playing: bool,
    shuffle: bool,
    repeat: RepeatMode,
    navigation: rivu_presentation::navigation::Navigation,
}

impl From<&TraySnapshot> for TrayMenuState {
    fn from(snapshot: &TraySnapshot) -> Self {
        Self {
            playing: snapshot.status == PlaybackStatus::Playing,
            shuffle: snapshot.shuffle,
            repeat: snapshot.repeat,
            navigation: snapshot.navigation,
        }
    }
}

struct TrayData {
    snapshot: TraySnapshot,
    menu_revision: u32,
}

impl TrayData {
    fn publish(&mut self, current: TraySnapshot) -> ((bool, Option<u32>), TraySnapshot) {
        let tooltip = tooltip_changed(&self.snapshot, &current);
        let menu_changed = TrayMenuState::from(&current) != TrayMenuState::from(&self.snapshot);
        let menu = menu_changed.then(|| {
            self.menu_revision = self.menu_revision.wrapping_add(1);
            self.menu_revision
        });
        let previous = std::mem::replace(&mut self.snapshot, current);
        ((tooltip, menu), previous)
    }

    fn menu_snapshot(&self) -> (TrayMenuState, u32) {
        (TrayMenuState::from(&self.snapshot), self.menu_revision)
    }
}

pub(crate) struct TrayController {
    cancel: Option<oneshot::Sender<()>>,
    worker: Option<async_executor::Task<()>>,
}

impl TrayController {
    pub(crate) fn start(
        handle: CoreHandle,
        app: AppHandle,
        i18n: Arc<Localizer>,
        executor: &Arc<async_executor::Executor<'static>>,
    ) -> Self {
        let (cancel, cancelled) = oneshot::channel();
        let worker = executor.spawn(run_bus(handle, app, i18n, cancelled, Arc::clone(executor)));
        Self {
            cancel: Some(cancel),
            worker: Some(worker),
        }
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(worker) = self.worker.take()
            && async_io::block_on(std::panic::AssertUnwindSafe(worker).catch_unwind()).is_err()
        {
            tracing::warn!("tray_service_panicked");
        }
    }
}

async fn run_bus(
    handle: CoreHandle,
    app: AppHandle,
    i18n: Arc<Localizer>,
    cancelled: oneshot::Receiver<()>,
    executor: Arc<async_executor::Executor<'static>>,
) {
    let cancelled = cancelled.shared();
    let updates = handle.subscribe_revisions(RevisionInterest::Tray);
    let data = Arc::new(RwLock::new(TrayData {
        snapshot: handle.tray_snapshot(),
        menu_revision: 1,
    }));
    // Reconcile after bus setup even when no later publication wakes this worker.
    let service_name = format!(
        "org.freedesktop.StatusNotifierItem-{}-{}",
        std::process::id(),
        INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let item = StatusNotifierItem {
        app: app.clone(),
        data: Arc::clone(&data),
        i18n: Arc::clone(&i18n),
    };
    let menu = DbusMenu {
        handle: handle.clone(),
        app,
        data: Arc::clone(&data),
        i18n,
    };
    // default-rt's shared native helper drives bus I/O independently of this
    // application's async-io executor, including registration and shutdown.
    let setup = async {
        Builder::session()
            .name(service_name.as_str())
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .serve_at(SNI_PATH, item)
            .serve_at(MENU_PATH, menu)
            .build()
            .await
    };
    let connection = match futures::select_biased! {
        _ = cancelled.clone().fuse() => None,
        connection = setup.fuse() => Some(connection),
    } {
        Some(Ok(connection)) => connection,
        Some(Err(error)) => {
            tracing::warn!(error = %error, "tray_unavailable: connecting to the session bus");
            return;
        }
        None => return,
    };

    let service = async {
        let item_address = connection
            .unique_name()
            .map(|name| name.as_str().to_owned())
            .unwrap_or_else(|| service_name.clone());
        let watcher_connection = connection.clone();
        let (watcher_stop, watcher_stopped) = oneshot::channel();
        let watcher = executor.spawn(watch_watcher(
            watcher_connection,
            item_address,
            watcher_stopped,
        ));

        if !apply_update(
            &data,
            &connection,
            handle.tray_snapshot(),
            cancelled.clone(),
        )
        .await
        {
            loop {
                futures::select_biased! {
                    _ = cancelled.clone().fuse() => break,
                    update = updates.recv().fuse() => {
                        if update.is_err()
                            || apply_update(
                                &data,
                                &connection,
                                handle.tray_snapshot(),
                                cancelled.clone(),
                            ).await
                        {
                            break;
                        }
                    }
                }
            }
        }
        // Cancel watcher setup and registration before closing the session socket.
        let _ = watcher_stop.send(());
        if std::panic::AssertUnwindSafe(watcher)
            .catch_unwind()
            .await
            .is_err()
        {
            tracing::warn!("tray_watcher_panicked");
        }
    };
    if std::panic::AssertUnwindSafe(service)
        .catch_unwind()
        .await
        .is_err()
    {
        tracing::warn!("tray_publisher_panicked");
    }
    // Disconnecting releases the item and service name without an unbounded drain.
    match rivu_protocol::transport::timeout(Duration::from_secs(1), connection.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(error = %error, "tray_connection_close_failed"),
        Err(_) => tracing::warn!("tray_connection_close_timed_out"),
    }
}

async fn watch_watcher(connection: Connection, service_name: String, stop: oneshot::Receiver<()>) {
    let monitor = async {
        let proxy = match fdo::DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(error) => {
                tracing::warn!(error = %error, "tray_watcher_monitor_unavailable");
                return;
            }
        };
        let mut owner_changes = match proxy
            .receive_name_owner_changed_with_args(&[(0, WATCHER_SERVICE)])
            .await
        {
            Ok(changes) => changes,
            Err(error) => {
                tracing::warn!(error = %error, "tray_watcher_monitor_unavailable");
                return;
            }
        };

        // Subscribe before the initial registration so a watcher restart during
        // setup cannot be missed. Registration is covered by cancellation too.
        if let Err(error) = register_with_watcher(&connection, &service_name).await {
            tracing::warn!(error = %error, "tray_watcher_not_registered");
        }
        while let Some(signal) = owner_changes.next().await {
            let Ok(args) = signal.args() else {
                continue;
            };
            if args.new_owner().is_some()
                && let Err(error) = register_with_watcher(&connection, &service_name).await
            {
                tracing::warn!(error = %error, "tray_watcher_reregistration_failed");
            }
        }
        owner_changes.async_drop().await;
    };
    // Cancellation covers setup and pending registration replies, not just the
    // idle signal wait. Dropping the stream queues RemoveMatch before shutdown.
    futures::select_biased! {
        _ = stop.fuse() => {},
        _ = monitor.fuse() => {},
    }
}

async fn register_with_watcher(connection: &Connection, service_name: &str) -> zbus::Result<()> {
    let watcher = Proxy::new(connection, WATCHER_SERVICE, WATCHER_PATH, WATCHER_INTERFACE).await?;
    let _: () = watcher
        .call("RegisterStatusNotifierItem", &(service_name,))
        .await?;
    Ok(())
}

async fn apply_update(
    data: &Arc<RwLock<TrayData>>,
    connection: &Connection,
    current: TraySnapshot,
    cancelled: Cancellation,
) -> bool {
    let shutting_down = current.shutting_down;
    let ((tooltip, menu), previous) = data.write().publish(current);
    drop(previous);
    if tooltip || menu.is_some() {
        futures::select_biased! {
            _ = cancelled.fuse() => return true,
            _ = emit_update(connection, tooltip, menu).fuse() => {},
        }
    }
    shutting_down
}

fn tooltip_changed(previous: &TraySnapshot, current: &TraySnapshot) -> bool {
    match (previous.current_track(), current.current_track()) {
        (Some(previous), Some(current)) => {
            !std::ptr::eq(previous, current)
                && (playback_title_changed(previous, current) || previous.artist != current.artist)
        }
        (None, None) => previous.status != current.status,
        _ => true,
    }
}

async fn emit_update(connection: &Connection, tooltip: bool, menu: Option<u32>) {
    if tooltip {
        let _ = connection
            .emit_signal(None::<&str>, SNI_PATH, SNI_INTERFACE, "NewToolTip", &())
            .await;
    }
    if let Some(menu_revision) = menu {
        let _ = connection
            .emit_signal(
                None::<&str>,
                MENU_PATH,
                MENU_INTERFACE,
                "LayoutUpdated",
                &(menu_revision, 0i32),
            )
            .await;
    }
}

struct StatusNotifierItem {
    app: AppHandle,
    data: Arc<RwLock<TrayData>>,
    i18n: Arc<Localizer>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    fn context_menu(&self, _x: i32, _y: i32) -> fdo::Result<()> {
        Ok(())
    }

    fn activate(&self, _x: i32, _y: i32) -> fdo::Result<()> {
        self.raise()
    }

    fn secondary_activate(&self, _x: i32, _y: i32) -> fdo::Result<()> {
        self.raise()
    }

    fn scroll(&self, _delta: i32, _orientation: &str) -> fdo::Result<()> {
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn category(&self) -> &'static str {
        "ApplicationStatus"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn id(&self) -> &'static str {
        "rivu"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn title(&self) -> &'static str {
        "Rivu"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn status(&self) -> &'static str {
        sni_status()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_name(&self) -> &'static str {
        "rivu"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn overlay_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn overlay_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn attention_icon_name(&self) -> &'static str {
        ""
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn attention_movie_name(&self) -> &'static str {
        ""
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn tool_tip(&self) -> (String, Vec<IconPixmap>, String, String) {
        let data = self.data.read();
        let snapshot = &data.snapshot;
        let description = match snapshot.current_track() {
            Some(track) if track.artist.is_empty() => playback_title(track).into_owned(),
            Some(track) => format!("{} — {}", playback_title(track), track.artist),
            None => self.i18n.playback_status(snapshot.status).to_owned(),
        };
        (
            "rivu".to_owned(),
            Vec::new(),
            "Rivu".to_owned(),
            description,
        )
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }
}

impl StatusNotifierItem {
    fn raise(&self) -> fdo::Result<()> {
        self.app
            .raise()
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }
}

#[derive(Debug, Serialize, Type)]
#[zbus(signature = "(ia{sv}av)")]
struct MenuLayout {
    id: i32,
    properties: HashMap<String, OwnedVariant>,
    children: Vec<Variant<'static>>,
}

impl MenuLayout {
    fn root(menu: TrayMenuState, recursion_depth: i32, i18n: &Localizer) -> Self {
        let children = if recursion_depth == 0 {
            Vec::new()
        } else {
            menu_items(menu, i18n).map(Variant::from).collect()
        };
        Self {
            id: 0,
            properties: HashMap::new(),
            children,
        }
    }
}

impl From<MenuLayout> for Variant<'static> {
    fn from(layout: MenuLayout) -> Self {
        let structure = StructureBuilder::new()
            .add_field(layout.id)
            .add_field(layout.properties)
            .add_field(layout.children)
            .build()
            .expect("menu layout structure is non-empty");
        Variant::from(structure)
    }
}

fn owned_variant(value: String) -> OwnedVariant {
    Variant::from(value)
        .try_to_owned()
        .expect("menu property should be ownable")
}

struct DbusMenu {
    handle: CoreHandle,
    app: AppHandle,
    data: Arc<RwLock<TrayData>>,
    i18n: Arc<Localizer>,
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> fdo::Result<(u32, MenuLayout)> {
        if parent_id != 0 {
            return Err(fdo::Error::InvalidArgs("Unknown menu parent".into()));
        }
        let (menu, revision) = self.data.read().menu_snapshot();
        Ok((
            revision,
            MenuLayout::root(menu, recursion_depth, &self.i18n),
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> fdo::Result<Vec<(i32, HashMap<String, OwnedVariant>)>> {
        let (menu, _) = self.data.read().menu_snapshot();
        let defaults = MenuItem::ALL.into_iter().filter(|_| ids.is_empty());
        Ok(defaults
            .chain(ids.iter().copied().filter_map(MenuItem::from_id))
            .map(|item| {
                (
                    item as i32,
                    item.properties(menu, &property_names, &self.i18n),
                )
            })
            .collect())
    }

    fn get_property(&self, id: i32, name: String) -> fdo::Result<OwnedVariant> {
        let (menu, _) = self.data.read().menu_snapshot();
        MenuItem::from_id(id)
            .and_then(|item| item.property(menu, &name, &self.i18n))
            .ok_or_else(|| fdo::Error::InvalidArgs("Unknown menu property".into()))
    }

    fn event(
        &self,
        id: i32,
        event_id: String,
        _data: OwnedVariant,
        _timestamp: u32,
    ) -> fdo::Result<()> {
        if event_id == "clicked"
            && let Some(item) = MenuItem::from_id(id)
        {
            self.activate(item);
        }
        Ok(())
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedVariant, u32)>) -> Vec<i32> {
        let mut invalid = Vec::new();
        for (id, event, _, _) in events {
            match MenuItem::from_id(id) {
                Some(item) if event == "clicked" => self.activate(item),
                None if id != 0 => invalid.push(id),
                _ => {}
            }
        }
        invalid
    }

    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let mut updates = Vec::new();
        let mut invalid = Vec::new();
        for id in ids {
            match self.about_to_show(id) {
                Ok(true) => updates.push(id),
                Ok(false) => {}
                Err(_) => invalid.push(id),
            }
        }
        (updates, invalid)
    }

    fn about_to_show(&self, id: i32) -> fdo::Result<bool> {
        if id == 0 {
            // Hosts commonly call AboutToShow before their first GetLayout.
            // Force the static root layout to be loaded instead of leaving an
            // empty host-side cache.
            Ok(true)
        } else if MenuItem::from_id(id).is_some() {
            Ok(false)
        } else {
            Err(fdo::Error::InvalidArgs("Unknown menu item".into()))
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn text_direction(&self) -> &'static str {
        "ltr"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn status(&self) -> &'static str {
        "normal"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

impl DbusMenu {
    fn activate(&self, item: MenuItem) {
        let command = match item {
            MenuItem::PlayPause => Command::Toggle,
            MenuItem::Previous => Command::Previous,
            MenuItem::Next => Command::Next,
            MenuItem::Shuffle => Command::ToggleShuffle,
            MenuItem::Repeat => Command::CycleRepeat,
            MenuItem::PlaybackSeparator | MenuItem::QuitSeparator => return,
            MenuItem::Quit => {
                if let Err(error) = self.app.request_shutdown() {
                    tracing::warn!(error = %error, "tray_quit_failed");
                }
                return;
            }
        };
        if let Err(error) = self.handle.send(command) {
            tracing::warn!(error = %error, "tray_command_failed");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
enum MenuItem {
    PlayPause = 2,
    Previous = 3,
    Next = 4,
    PlaybackSeparator = 5,
    Shuffle = 6,
    Repeat = 7,
    QuitSeparator = 8,
    Quit = 9,
}

impl MenuItem {
    const ALL: [Self; 8] = [
        Self::PlayPause,
        Self::Previous,
        Self::Next,
        Self::PlaybackSeparator,
        Self::Shuffle,
        Self::Repeat,
        Self::QuitSeparator,
        Self::Quit,
    ];

    fn from_id(id: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|item| *item as i32 == id)
    }

    fn label(self, menu: TrayMenuState, i18n: &Localizer) -> &str {
        i18n.text(match self {
            Self::PlayPause if menu.playing => "core-pause",
            Self::PlayPause => "playback-play",
            Self::Previous => "core-previous",
            Self::Next => "core-next",
            Self::Shuffle => "core-shuffle",
            Self::Repeat => match menu.repeat {
                RepeatMode::Off => "core-repeat-off",
                RepeatMode::All => "core-repeat-all",
                RepeatMode::One => "core-repeat-one",
            },
            Self::Quit => "app-quit",
            Self::PlaybackSeparator | Self::QuitSeparator => return "",
        })
    }

    fn property(self, menu: TrayMenuState, name: &str, i18n: &Localizer) -> Option<OwnedVariant> {
        Some(match name {
            "label" => owned_variant(self.label(menu, i18n).to_owned()),
            "type" => owned_variant(
                if matches!(self, Self::PlaybackSeparator | Self::QuitSeparator) {
                    "separator"
                } else {
                    "standard"
                }
                .to_owned(),
            ),
            "enabled" if self == Self::Previous => menu.navigation.previous.into(),
            "enabled" if self == Self::Next => menu.navigation.next.into(),
            "enabled" | "visible" => OwnedVariant::from(true),
            "toggle-type" if self == Self::Shuffle => owned_variant("checkmark".to_owned()),
            "toggle-state" if self == Self::Shuffle => OwnedVariant::from(i32::from(menu.shuffle)),
            _ => return None,
        })
    }

    fn properties(
        self,
        menu: TrayMenuState,
        requested: &[String],
        i18n: &Localizer,
    ) -> HashMap<String, OwnedVariant> {
        const PROPERTIES: [&str; 6] = [
            "label",
            "type",
            "enabled",
            "visible",
            "toggle-type",
            "toggle-state",
        ];
        PROPERTIES
            .into_iter()
            .filter(|name| {
                requested.is_empty() || requested.iter().any(|requested| requested == name)
            })
            .filter_map(|name| {
                self.property(menu, name, i18n)
                    .map(|value| (name.to_owned(), value))
            })
            .collect()
    }
}

fn menu_items(menu: TrayMenuState, i18n: &Localizer) -> impl Iterator<Item = MenuLayout> + '_ {
    MenuItem::ALL.into_iter().map(move |item| MenuLayout {
        id: item as i32,
        properties: item.properties(menu, &[], i18n),
        children: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_menu_quit_stops_core_without_window_and_reports_invalid_ids() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut runtime = crate::core::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let handle = runtime.handle.clone();
        let app = AppHandle::new(&runtime, directory.path());
        let menu = DbusMenu {
            data: Arc::new(RwLock::new(TrayData {
                snapshot: handle.tray_snapshot(),
                menu_revision: 1,
            })),
            i18n: Arc::new(Localizer::new(&handle.config_snapshot().ui_language)),
            handle: handle.clone(),
            app,
        };
        assert_eq!(
            menu.about_to_show_group(vec![0, 1, 2, 9, 99, -1]),
            (vec![0], vec![1, 99, -1]),
        );
        assert_eq!(
            menu.event_group(vec![
                (1, "clicked".into(), OwnedVariant::from(0_i32), 0),
                (99, "clicked".into(), OwnedVariant::from(0_i32), 0),
                (9, "clicked".into(), OwnedVariant::from(0_i32), 0),
            ]),
            vec![1, 99],
        );
        runtime.join().unwrap();
        assert!(handle.is_shutting_down());
    }

    #[test]
    fn shuffle_menu_item_uses_native_checkmark_without_changing_label() {
        let i18n = Localizer::new("en-US");
        let mut menu = TrayMenuState {
            playing: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            navigation: Default::default(),
        };
        let label = owned_variant(i18n.text("core-shuffle").to_owned());
        let checkmark = owned_variant("checkmark".to_owned());
        for (shuffle, state) in [(false, 0_i32), (true, 1_i32)] {
            menu.shuffle = shuffle;
            let item = menu_items(menu, &i18n).find(|item| item.id == 6).unwrap();
            let properties = &item.properties;
            assert_eq!(properties.get("label"), Some(&label));
            assert_eq!(properties.get("toggle-type"), Some(&checkmark));
            assert_eq!(
                properties.get("toggle-state"),
                Some(&OwnedVariant::from(state))
            );
        }
    }

    #[test]
    fn tooltip_updates_follow_visible_text_not_playback_statistics() {
        let mut state = crate::core::CoreState {
            current_track: Some(Arc::new(rivu_protocol::test_support::artwork_track(
                "track.wav".into(),
            ))),
            ..Default::default()
        };
        let mut data = TrayData {
            snapshot: TraySnapshot::from_core(&state),
            menu_revision: 1,
        };
        state.playback.position = 12.0;
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (false, None)
        );
        let track = Arc::make_mut(state.current_track.as_mut().unwrap());
        track.play_count += 1;
        track.last_played = Some(42);
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (false, None)
        );
        state.playback.status = PlaybackStatus::Playing;
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (false, Some(2))
        );
        Arc::make_mut(state.current_track.as_mut().unwrap())
            .title
            .push_str(" renamed");
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, None)
        );
        Arc::make_mut(state.current_track.as_mut().unwrap())
            .artist
            .push_str(" artist");
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, None)
        );
        let current = Arc::make_mut(state.current_track.as_mut().unwrap());
        current.path = "renamed.wav".into();
        assert!(!data.publish(TraySnapshot::from_core(&state)).0.0);
        let current = Arc::make_mut(state.current_track.as_mut().unwrap());
        current.title.clear();
        assert!(data.publish(TraySnapshot::from_core(&state)).0.0);
        let current = Arc::make_mut(state.current_track.as_mut().unwrap());
        current.path = "side-b.wav".into();
        assert!(data.publish(TraySnapshot::from_core(&state)).0.0);
        let current = Arc::make_mut(state.current_track.as_mut().unwrap());
        current.cue = Some(rivu_protocol::model::CueSegment {
            sheet: "disc.cue".into(),
            number: 2,
            start_frame: 0,
            end_frame: None,
        });
        assert_eq!(playback_title(current), "side-b · #02");
        assert!(data.publish(TraySnapshot::from_core(&state)).0.0);
        let track = state.current_track.take();
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, None)
        );
        state.current_track = track;
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, None)
        );
        state.current_track = None;
        data.publish(TraySnapshot::from_core(&state));
        state.playback.status = PlaybackStatus::Paused;
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, Some(3))
        );
        state.playback.status = PlaybackStatus::Stopped;
        assert_eq!(
            data.publish(TraySnapshot::from_core(&state)).0,
            (true, None)
        );
    }

    #[test]
    fn background_setup_reconciles_published_baseline_and_layout_revision_stays_paired() {
        let baseline = TraySnapshot {
            current_track: None,
            status: PlaybackStatus::Stopped,
            shuffle: false,
            repeat: RepeatMode::Off,
            navigation: Default::default(),
            shutting_down: false,
        };
        let i18n = Localizer::new("en-US");
        let mut data = TrayData {
            snapshot: baseline.clone(),
            menu_revision: 1,
        };
        // Playback may change during bus setup, before another wakeup arrives.
        let mut latest = baseline;
        latest.status = PlaybackStatus::Playing;
        assert_eq!(data.publish(latest.clone()).0, (true, Some(2)));
        let (layout_menu, layout_revision) = data.menu_snapshot();
        assert_eq!(layout_revision, 2);
        // A newer publication cannot alter the already captured layout/revision.
        latest.status = PlaybackStatus::Paused;
        assert_eq!(data.publish(latest.clone()).0, (true, Some(3)));
        assert_eq!(
            MenuItem::PlayPause.label(layout_menu, &i18n),
            i18n.text("core-pause")
        );
        let (paused_menu, paused_revision) = data.menu_snapshot();
        assert_eq!(paused_revision, 3);
        assert_eq!(
            MenuItem::PlayPause.label(paused_menu, &i18n),
            i18n.text("playback-play")
        );
        assert_eq!(data.publish(latest.clone()).0, (false, None));
        latest.shutting_down = true;
        assert_eq!(data.publish(latest.clone()).0, (false, None));
        assert!(data.snapshot.shutting_down);
        assert_eq!(data.menu_revision, 3);
        data.menu_revision = u32::MAX;
        latest.shuffle = true;
        assert_eq!(data.publish(latest).0, (false, Some(0)));
        let (menu, revision) = data.menu_snapshot();
        let root_only = MenuLayout::root(menu, 0, &i18n);
        assert_eq!(revision, 0);
        assert!(root_only.children.is_empty());
    }
}

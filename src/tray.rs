use crate::{
    app::AppHandle, artwork::DEFAULT_IMAGE, core::CoreHandle, i18n::Localizer,
    projection::TraySnapshot,
};
use futures::{FutureExt, StreamExt, channel::oneshot, future::Shared};
use parking_lot::RwLock;
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
    AsyncDrop, Connection, Proxy,
    connection::Builder,
    fdo,
    zvariant::{ObjectPath, OwnedValue, StructureBuilder, Type, Value},
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

fn icon_pixmap() -> &'static Vec<IconPixmap> {
    static ICON: std::sync::LazyLock<Vec<IconPixmap>> = std::sync::LazyLock::new(|| {
        let image = image::load_from_memory(DEFAULT_IMAGE)
            .expect("embedded Rivu tray icon")
            .to_rgba8();
        let (width, height) = image.dimensions();
        let pixels = image
            .pixels()
            .flat_map(|pixel| [pixel[3], pixel[0], pixel[1], pixel[2]])
            .collect();
        vec![(width as i32, height as i32, pixels)]
    });
    &ICON
}

struct TrayData {
    i18n: Arc<Localizer>,
    snapshot: TraySnapshot,
    menu_revision: u32,
}

impl TrayData {
    fn publish(&mut self, current: &TraySnapshot) -> Option<(TraySnapshot, u32, bool)> {
        if !current.changed_from(&self.snapshot) {
            return None;
        }
        let menu_changed = current.menu_changed_from(&self.snapshot);
        if menu_changed {
            self.menu_revision = self.menu_revision.wrapping_add(1);
        }
        let previous = std::mem::replace(&mut self.snapshot, current.clone());
        Some((previous, self.menu_revision, menu_changed))
    }

    fn layout_snapshot(&self) -> (TraySnapshot, u32) {
        (self.snapshot.clone(), self.menu_revision)
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
    let data = Arc::new(RwLock::new(TrayData {
        i18n,
        snapshot: handle.tray_snapshot(),
        menu_revision: 1,
    }));
    // Subscribe after the published baseline, before bus setup. Reconcile even
    // when no later publication wakes this worker.
    let updates = handle.subscribe_revisions();
    let service_name = format!(
        "org.freedesktop.StatusNotifierItem-{}-{}",
        std::process::id(),
        INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let item = StatusNotifierItem {
        app: app.clone(),
        data: Arc::clone(&data),
    };
    let menu = DbusMenu {
        handle: handle.clone(),
        app,
        data: Arc::clone(&data),
    };
    let setup = async {
        Builder::session()?
            .name(service_name.as_str())?
            .serve_at(SNI_PATH, item)?
            .serve_at(MENU_PATH, menu)?
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
    let update = data.write().publish(&current);
    if let Some((previous, revision, menu_changed)) = update {
        futures::select_biased! {
            _ = cancelled.fuse() => return true,
            _ = emit_update(connection, &previous, &current, revision, menu_changed).fuse() => {},
        }
    }
    current.shutting_down
}

async fn emit_update(
    connection: &Connection,
    previous: &TraySnapshot,
    current: &TraySnapshot,
    menu_revision: u32,
    menu_changed: bool,
) {
    let track_changed = match (&previous.current_track, &current.current_track) {
        (None, None) => false,
        (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
        _ => true,
    };
    if previous.current_track_id != current.current_track_id || track_changed {
        let _ = connection
            .emit_signal(None::<&str>, SNI_PATH, SNI_INTERFACE, "NewToolTip", &())
            .await;
    }
    if menu_changed {
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
        ""
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        icon_pixmap().clone()
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
            Some(track) if track.artist.is_empty() => track.title.clone(),
            Some(track) => format!("{} — {}", track.title, track.artist),
            None => match snapshot.status {
                PlaybackStatus::Playing => data.i18n.text("core-playing").to_owned(),
                PlaybackStatus::Paused => data.i18n.text("core-paused").to_owned(),
                PlaybackStatus::Stopped => data.i18n.text("core-stopped").to_owned(),
            },
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
#[zvariant(signature = "(ia{sv}av)")]
struct MenuLayout {
    id: i32,
    properties: HashMap<String, OwnedValue>,
    children: Vec<Value<'static>>,
}

impl MenuLayout {
    fn root(snapshot: &TraySnapshot, recursion_depth: i32, i18n: &Localizer) -> Self {
        let children = if recursion_depth == 0 {
            Vec::new()
        } else {
            menu_items(snapshot, i18n)
                .into_iter()
                .map(Value::from)
                .collect()
        };
        Self {
            id: 0,
            properties: HashMap::new(),
            children,
        }
    }

    fn item(id: i32, label: impl Into<String>) -> Self {
        let mut properties = HashMap::new();
        properties.insert("label".to_owned(), owned_value(label.into()));
        properties.insert("enabled".to_owned(), OwnedValue::from(true));
        properties.insert("visible".to_owned(), OwnedValue::from(true));
        Self {
            id,
            properties,
            children: Vec::new(),
        }
    }

    fn separator(id: i32) -> Self {
        let mut properties = HashMap::new();
        properties.insert("type".to_owned(), owned_value("separator".to_owned()));
        Self {
            id,
            properties,
            children: Vec::new(),
        }
    }
}

impl From<MenuLayout> for Value<'static> {
    fn from(layout: MenuLayout) -> Self {
        let structure = StructureBuilder::new()
            .add_field(layout.id)
            .add_field(layout.properties)
            .add_field(layout.children)
            .build()
            .expect("menu layout structure is non-empty");
        Value::from(structure)
    }
}

fn owned_value(value: String) -> OwnedValue {
    Value::from(value)
        .try_to_owned()
        .expect("menu property should be ownable")
}

struct DbusMenu {
    handle: CoreHandle,
    app: AppHandle,
    data: Arc<RwLock<TrayData>>,
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
        let data = self.data.read();
        let (snapshot, revision) = data.layout_snapshot();
        Ok((
            revision,
            MenuLayout::root(&snapshot, recursion_depth, &data.i18n),
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> fdo::Result<Vec<(i32, HashMap<String, OwnedValue>)>> {
        let snapshot = self.data.read().snapshot.clone();
        let ids = if ids.is_empty() {
            (2..=9).collect()
        } else {
            ids
        };
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                menu_properties(&snapshot, id, &property_names, &self.data.read().i18n)
                    .map(|props| (id, props))
            })
            .collect())
    }

    fn get_property(&self, id: i32, name: String) -> fdo::Result<OwnedValue> {
        let snapshot = self.data.read().snapshot.clone();
        menu_properties(&snapshot, id, &[name], &self.data.read().i18n)
            .and_then(|mut properties| properties.drain().next().map(|(_, value)| value))
            .ok_or_else(|| fdo::Error::InvalidArgs("Unknown menu property".into()))
    }

    fn event(
        &self,
        id: i32,
        event_id: String,
        _data: OwnedValue,
        _timestamp: u32,
    ) -> fdo::Result<()> {
        if event_id == "clicked" {
            self.activate(id);
        }
        Ok(())
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let mut invalid = Vec::new();
        for (id, event, _, _) in events {
            if !(2..=9).contains(&id) && id != 0 {
                invalid.push(id);
            } else if event == "clicked" {
                self.activate(id);
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
        } else if (2..=9).contains(&id) {
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
    fn activate(&self, id: i32) {
        if id == 9 {
            if let Err(error) = self.app.request_user_shutdown() {
                tracing::warn!(error = %error, "tray_quit_failed");
            }
            return;
        }
        let snapshot = self.data.read().snapshot.clone();
        let command = match id {
            2 if snapshot.status == PlaybackStatus::Playing => Command::Pause,
            2 => Command::Resume,
            3 => Command::Previous,
            4 => Command::Next,
            6 => Command::ToggleShuffle,
            7 => Command::CycleRepeat,
            _ => return,
        };
        if let Err(error) = self.handle.send(command) {
            tracing::warn!(error = %error, "tray_command_failed");
        }
    }
}

fn menu_items(snapshot: &TraySnapshot, i18n: &Localizer) -> Vec<MenuLayout> {
    let play_label = if snapshot.status == PlaybackStatus::Playing {
        i18n.text("core-pause")
    } else {
        i18n.text("core-play")
    };
    vec![
        MenuLayout::item(2, play_label),
        MenuLayout::item(3, i18n.text("core-previous")),
        MenuLayout::item(4, i18n.text("core-next")),
        MenuLayout::separator(5),
        MenuLayout::item(
            6,
            if snapshot.shuffle {
                i18n.text("core-shuffle-on")
            } else {
                i18n.text("core-shuffle")
            },
        ),
        MenuLayout::item(
            7,
            if snapshot.repeat == RepeatMode::Off {
                i18n.text("core-repeat")
            } else {
                i18n.text("core-repeat-on")
            },
        ),
        MenuLayout::separator(8),
        MenuLayout::item(9, i18n.text("core-quit")),
    ]
}

fn menu_properties(
    snapshot: &TraySnapshot,
    id: i32,
    requested: &[String],
    i18n: &Localizer,
) -> Option<HashMap<String, OwnedValue>> {
    if !(2..=9).contains(&id) {
        return None;
    }
    let play_label = if snapshot.status == PlaybackStatus::Playing {
        i18n.text("core-pause")
    } else {
        i18n.text("core-play")
    };
    let (label, item_type) = match id {
        2 => (play_label, "standard"),
        3 => (i18n.text("core-previous"), "standard"),
        4 => (i18n.text("core-next"), "standard"),
        5 | 8 => ("", "separator"),
        6 => (
            if snapshot.shuffle {
                i18n.text("core-shuffle-on")
            } else {
                i18n.text("core-shuffle")
            },
            "standard",
        ),
        7 => (
            if snapshot.repeat == RepeatMode::Off {
                i18n.text("core-repeat")
            } else {
                i18n.text("core-repeat-on")
            },
            "standard",
        ),
        9 => (i18n.text("core-quit"), "standard"),
        _ => unreachable!(),
    };
    let all = requested.is_empty();
    let mut properties = HashMap::new();
    if all || requested.iter().any(|name| name == "label") {
        properties.insert("label".to_owned(), owned_value(label.to_owned()));
    }
    if all || requested.iter().any(|name| name == "type") {
        properties.insert("type".to_owned(), owned_value(item_type.to_owned()));
    }
    if all || requested.iter().any(|name| name == "enabled") {
        properties.insert("enabled".to_owned(), OwnedValue::from(true));
    }
    if all || requested.iter().any(|name| name == "visible") {
        properties.insert("visible".to_owned(), OwnedValue::from(true));
    }
    Some(properties)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_menu_quit_stops_core_without_window_and_reports_invalid_ids() {
        let directory = tempfile::tempdir().unwrap();
        let mut runtime = crate::core::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let handle = runtime.handle.clone();
        let app = AppHandle::new(&runtime, false);
        assert!(!app.can_raise());
        let menu = DbusMenu {
            data: Arc::new(RwLock::new(TrayData {
                i18n: Arc::new(Localizer::new(&handle.config_snapshot().ui_language)),
                snapshot: handle.tray_snapshot(),
                menu_revision: 1,
            })),
            handle: handle.clone(),
            app,
        };
        assert_eq!(
            menu.about_to_show_group(vec![0, 1, 2, 9, 99, -1]),
            (vec![0], vec![1, 99, -1]),
        );
        assert_eq!(
            menu.event_group(vec![
                (1, "clicked".into(), OwnedValue::from(0_i32), 0),
                (99, "clicked".into(), OwnedValue::from(0_i32), 0),
                (9, "clicked".into(), OwnedValue::from(0_i32), 0),
            ]),
            vec![1, 99],
        );
        runtime.join().unwrap();
        assert!(handle.is_shutting_down());
    }

    #[test]
    fn background_setup_reconciles_published_baseline_and_layout_revision_stays_paired() {
        let baseline = TraySnapshot {
            current_track: None,
            current_track_id: None,
            status: PlaybackStatus::Stopped,
            shuffle: false,
            repeat: RepeatMode::Off,
            shutting_down: false,
        };
        let mut data = TrayData {
            i18n: Arc::new(Localizer::new("en-US")),
            snapshot: baseline.clone(),
            menu_revision: 1,
        };
        // Playback may change during bus setup, before another wakeup arrives.
        let mut latest = baseline;
        latest.status = PlaybackStatus::Playing;
        let (previous, revision, menu_changed) = data.publish(&latest).unwrap();
        assert_eq!(previous.status, PlaybackStatus::Stopped);
        assert!(menu_changed);
        assert_eq!(revision, 2);
        let (layout_snapshot, layout_revision) = data.layout_snapshot();
        assert_eq!(layout_revision, revision);
        // A newer publication cannot alter the already captured layout/revision.
        latest.status = PlaybackStatus::Paused;
        assert_eq!(data.publish(&latest).unwrap().1, 3);
        assert_eq!(layout_snapshot.status, PlaybackStatus::Playing);
        assert!(data.publish(&latest).is_none());
        latest.shutting_down = true;
        let (_, revision, menu_changed) = data.publish(&latest).unwrap();
        assert!(!menu_changed);
        assert_eq!(revision, 3);
        data.menu_revision = u32::MAX;
        latest.shuffle = true;
        assert_eq!(data.publish(&latest).unwrap().1, 0);
        let (snapshot, revision) = data.layout_snapshot();
        let root_only = MenuLayout::root(&snapshot, 0, &data.i18n);
        assert_eq!(revision, 0);
        assert!(root_only.children.is_empty());
    }
}

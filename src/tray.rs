use crate::{
    artwork::DEFAULT_IMAGE,
    core::AppHandle,
    model::{Command, PlaybackStatus},
    projection::TraySnapshot,
};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded, select_biased};
use parking_lot::RwLock;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};
use zbus::{
    blocking::{Connection, Proxy, connection::Builder, fdo::DBusProxy},
    fdo,
    zvariant::{ObjectPath, OwnedValue, StructureBuilder, Type, Value},
};

const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const SNI_PATH: &str = "/StatusNotifierItem";
const SNI_INTERFACE: &str = "org.freedesktop.StatusNotifierItem";
const MENU_PATH: &str = "/Menu";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(1);
type IconPixmap = (i32, i32, Vec<u8>);

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
    snapshot: TraySnapshot,
    menu_revision: u32,
}

pub(crate) struct TrayController {
    stop: Option<Sender<()>>,
    connection: Option<Connection>,
    worker: Option<JoinHandle<()>>,
}

impl TrayController {
    pub(crate) fn start(handle: AppHandle) -> Result<Option<Self>> {
        let (stop, stop_receiver) = bounded(1);
        let (ready_sender, ready_receiver) = bounded(1);
        let data = Arc::new(RwLock::new(TrayData {
            snapshot: handle.tray_snapshot(),
            menu_revision: 1,
        }));
        let worker = match thread::Builder::new()
            .name("rivu-tray".into())
            .spawn(move || run_bus(handle, data, stop_receiver, ready_sender))
        {
            Ok(worker) => worker,
            Err(error) => {
                tracing::warn!(error = %error, "tray_unavailable: could not start D-Bus thread");
                return Ok(None);
            }
        };

        match ready_receiver.recv() {
            Ok(Ok(connection)) => Ok(Some(Self {
                stop: Some(stop),
                connection: Some(connection),
                worker: Some(worker),
            })),
            Ok(Err(error)) => {
                let _ = worker.join();
                tracing::warn!(error = %error, "tray_unavailable");
                Ok(None)
            }
            Err(error) => {
                let _ = worker.join();
                tracing::warn!(error = %error, "tray_unavailable: startup thread stopped");
                Ok(None)
            }
        }
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.try_send(());
        }
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_bus(
    handle: AppHandle,
    data: Arc<RwLock<TrayData>>,
    stop: Receiver<()>,
    ready: Sender<std::result::Result<Connection, String>>,
) {
    let service_name = format!(
        "org.freedesktop.StatusNotifierItem-{}-{}",
        std::process::id(),
        INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let item = StatusNotifierItem {
        handle: handle.clone(),
        data: Arc::clone(&data),
    };
    let menu = DbusMenu {
        handle: handle.clone(),
        data: Arc::clone(&data),
    };
    let connection = match Builder::session()
        .and_then(|builder| builder.name(service_name.as_str()))
        .and_then(|builder| builder.serve_at(SNI_PATH, item))
        .and_then(|builder| builder.serve_at(MENU_PATH, menu))
        .and_then(|builder| builder.build())
        .map_err(|error| format!("Connecting to the session bus: {error}"))
    {
        Ok(connection) => connection,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let item_address = connection
        .unique_name()
        .map(|name| format!("{}{SNI_PATH}", name.as_str()))
        .unwrap_or_else(|| service_name.clone());
    if let Err(error) = register_with_watcher(&connection, &item_address) {
        tracing::warn!(error = %error, "tray_watcher_unavailable");
    }

    let updates = handle.subscribe();
    let updater_connection = connection.clone();
    let updater_data = Arc::clone(&data);
    let updater_handle = handle.clone();
    let updater = match thread::Builder::new()
        .name("rivu-tray-updates".into())
        .spawn(move || {
            update_loop(
                updater_handle,
                updater_data,
                updater_connection,
                updates,
                stop,
            )
        }) {
        Ok(updater) => updater,
        Err(error) => {
            let _ = ready.send(Err(format!("Starting tray update thread: {error}")));
            let _ = connection.close();
            return;
        }
    };

    let watcher_connection = connection.clone();
    let watcher_item_address = connection
        .unique_name()
        .map(|name| format!("{}{SNI_PATH}", name.as_str()))
        .unwrap_or(service_name);
    let watcher = match thread::Builder::new()
        .name("rivu-tray-watcher".into())
        .spawn(move || watch_watcher(watcher_connection, watcher_item_address))
    {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(error = %error, "tray_watcher_monitor_unavailable");
            None
        }
    };

    if ready.send(Ok(connection.clone())).is_err() {
        let _ = connection.close();
        let _ = updater.join();
        if let Some(watcher) = watcher {
            let _ = watcher.join();
        }
        return;
    }
    connection.closed();
    let _ = updater.join();
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
}

fn watch_watcher(connection: Connection, service_name: String) {
    let proxy = match DBusProxy::new(&connection) {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::warn!(error = %error, "tray_watcher_monitor_unavailable");
            return;
        }
    };
    let owner_changes = match proxy.receive_name_owner_changed_with_args(&[(0, WATCHER_SERVICE)]) {
        Ok(changes) => changes,
        Err(error) => {
            tracing::warn!(error = %error, "tray_watcher_monitor_unavailable");
            return;
        }
    };

    // The watcher may have appeared between the initial registration attempt and
    // signal subscription. Retry once before waiting for future owner changes.
    if let Err(error) = register_with_watcher(&connection, &service_name) {
        tracing::warn!(error = %error, "tray_watcher_not_registered");
    }
    for signal in owner_changes {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.new_owner().is_some()
            && let Err(error) = register_with_watcher(&connection, &service_name)
        {
            tracing::warn!(error = %error, "tray_watcher_reregistration_failed");
        }
    }
}

fn register_with_watcher(connection: &Connection, service_name: &str) -> zbus::Result<()> {
    let watcher = Proxy::new(connection, WATCHER_SERVICE, WATCHER_PATH, WATCHER_INTERFACE)?;
    let _: () = watcher.call("RegisterStatusNotifierItem", &(service_name,))?;
    Ok(())
}

fn update_loop(
    handle: AppHandle,
    data: Arc<RwLock<TrayData>>,
    connection: Connection,
    updates: Receiver<()>,
    stop: Receiver<()>,
) {
    let mut previous = handle.tray_snapshot();
    select_biased! {
        recv(stop) -> _ => return,
        recv(updates) -> update => {
            if update.is_err() {
                return;
            }
            let current = handle.tray_snapshot();
            if apply_update(&data, &connection, &mut previous, current) {
                return;
            }
        }
    }
    loop {
        select_biased! {
            recv(stop) -> _ => return,
            recv(updates) -> update => {
                if update.is_err() {
                    return;
                }
                let current = handle.tray_snapshot();
                if apply_update(&data, &connection, &mut previous, current) {
                    return;
                }
            }
        }
    }
}

fn apply_update(
    data: &Arc<RwLock<TrayData>>,
    connection: &Connection,
    previous: &mut TraySnapshot,
    current: TraySnapshot,
) -> bool {
    if current.changed_from(previous) {
        let menu_changed = current.menu_changed_from(previous);
        let revision = {
            let mut state = data.write();
            state.snapshot = current.clone();
            if menu_changed {
                state.menu_revision = state.menu_revision.wrapping_add(1);
            }
            state.menu_revision
        };
        emit_update(connection, previous, &current, revision, menu_changed);
        *previous = current;
    }
    previous.shutting_down
}

fn emit_update(
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
        let _ = connection.emit_signal(None::<&str>, SNI_PATH, SNI_INTERFACE, "NewToolTip", &());
    }
    if menu_changed {
        let _ = connection.emit_signal(
            None::<&str>,
            MENU_PATH,
            MENU_INTERFACE,
            "LayoutUpdated",
            &(menu_revision, 0i32),
        );
    }
}

struct StatusNotifierItem {
    handle: AppHandle,
    data: Arc<RwLock<TrayData>>,
}

#[zbus::interface(name = "org.freedesktop.StatusNotifierItem")]
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
    fn window_id(&self) -> u32 {
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
        let snapshot = self.data.read().snapshot.clone();
        let description = match snapshot.current_track() {
            Some(track) if track.artist.is_empty() => track.title.clone(),
            Some(track) => format!("{} — {}", track.title, track.artist),
            None => match snapshot.status {
                PlaybackStatus::Playing => "Playing".to_owned(),
                PlaybackStatus::Paused => "Paused".to_owned(),
                PlaybackStatus::Stopped => "Stopped".to_owned(),
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
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }
}

impl StatusNotifierItem {
    fn raise(&self) -> fdo::Result<()> {
        self.handle
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
    handle: AppHandle,
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
        let snapshot = self.data.read().snapshot.clone();
        let children = if recursion_depth == 0 {
            Vec::new()
        } else {
            menu_items(&snapshot).into_iter().map(Value::from).collect()
        };
        Ok((
            self.data.read().menu_revision,
            MenuLayout {
                id: 0,
                properties: HashMap::new(),
                children,
            },
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> fdo::Result<Vec<(i32, HashMap<String, OwnedValue>)>> {
        let snapshot = self.data.read().snapshot.clone();
        let ids = if ids.is_empty() {
            (1..=6).collect()
        } else {
            ids
        };
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                menu_properties(&snapshot, id, &property_names).map(|props| (id, props))
            })
            .collect())
    }

    fn get_property(&self, id: i32, name: String) -> fdo::Result<OwnedValue> {
        let snapshot = self.data.read().snapshot.clone();
        menu_properties(&snapshot, id, &[name])
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

    fn about_to_show(&self, id: i32) -> fdo::Result<bool> {
        if id == 0 {
            // Hosts commonly call AboutToShow before their first GetLayout.
            // Force the static root layout to be loaded instead of leaving an
            // empty host-side cache.
            Ok(true)
        } else if (1..=6).contains(&id) {
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
        let snapshot = self.data.read().snapshot.clone();
        let command = match id {
            1 => {
                if let Err(error) = self.handle.raise() {
                    tracing::warn!(error = %error, "tray_show_window_failed");
                }
                return;
            }
            2 if snapshot.status == PlaybackStatus::Playing => Command::Pause,
            2 => Command::Resume,
            3 => Command::Previous,
            4 => Command::Next,
            6 => Command::Shutdown,
            _ => return,
        };
        if let Err(error) = self.handle.send(command) {
            tracing::warn!(error = %error, "tray_command_failed");
        }
    }
}

fn menu_items(snapshot: &TraySnapshot) -> Vec<MenuLayout> {
    let play_label = if snapshot.status == PlaybackStatus::Playing {
        "Pause"
    } else {
        "Play"
    };
    vec![
        MenuLayout::item(1, "Show Rivu"),
        MenuLayout::item(2, play_label),
        MenuLayout::item(3, "Previous"),
        MenuLayout::item(4, "Next"),
        MenuLayout::separator(5),
        MenuLayout::item(6, "Quit Rivu"),
    ]
}

fn menu_properties(
    snapshot: &TraySnapshot,
    id: i32,
    requested: &[String],
) -> Option<HashMap<String, OwnedValue>> {
    if !(1..=6).contains(&id) {
        return None;
    }
    let play_label = if snapshot.status == PlaybackStatus::Playing {
        "Pause"
    } else {
        "Play"
    };
    let (label, item_type) = match id {
        1 => ("Show Rivu", "standard"),
        2 => (play_label, "standard"),
        3 => ("Previous", "standard"),
        4 => ("Next", "standard"),
        5 => ("", "separator"),
        6 => ("Quit Rivu", "standard"),
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
    fn menu_labels_follow_playback_status() {
        let mut snapshot = TraySnapshot {
            current_track: None,
            current_track_id: None,
            status: PlaybackStatus::Playing,
            shutting_down: false,
        };
        assert_eq!(
            menu_properties(&snapshot, 2, &[]).unwrap()["label"],
            owned_value("Pause".into())
        );
        snapshot.status = PlaybackStatus::Paused;
        assert_eq!(
            menu_properties(&snapshot, 2, &[]).unwrap()["label"],
            owned_value("Play".into())
        );
        snapshot.status = PlaybackStatus::Stopped;
        assert_eq!(
            menu_properties(&snapshot, 2, &[]).unwrap()["label"],
            owned_value("Play".into())
        );
    }

    #[test]
    fn menu_contains_fixed_actions_and_active_status_is_independent() {
        let snapshot = TraySnapshot {
            current_track: None,
            current_track_id: None,
            status: PlaybackStatus::Stopped,
            shutting_down: false,
        };
        let items = menu_items(&snapshot);
        let labels: Vec<_> = items
            .iter()
            .filter_map(|item| item.properties.get("label"))
            .collect();
        assert_eq!(labels.len(), 5);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[1].id, 2);
        assert_eq!(items[2].id, 3);
        assert_eq!(items[3].id, 4);
        assert_eq!(items[4].id, 5);
        assert_eq!(items[5].id, 6);
        assert_eq!(sni_status(), "Active");
    }
}

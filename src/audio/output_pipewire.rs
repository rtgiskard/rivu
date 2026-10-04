//! Native PipeWire streams with a fixed target and explicit speaker positions.
//! CPAL's ALSA handle cannot call set_chmap; probing a separate handle does not
//! change the stream it opens. In particular ALSA's default 7ch map is UNKNOWN.

use super::{Channel, OutputCallback, layout};
use ::pipewire::{self as pw, properties::properties, spa};
use anyhow::{Context, Result, bail};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub(super) const PREFIX: &str = "PipeWire: ";
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
struct Sink {
    id: u32,
    serial: String,
    name: String,
    positions: String,
}

struct Route {
    output_node: u32,
    output_port: u32,
    input_node: u32,
    input_port: u32,
}

#[derive(Default)]
struct Inventory {
    sinks: BTreeMap<u32, Sink>,
    default: Option<String>,
    ports: BTreeMap<u32, String>,
    links: BTreeMap<u32, Route>,
    revision: u64,
}

struct Connection {
    _listener: pw::registry::Listener,
    _proxies: Rc<RefCell<Vec<(Box<dyn pw::proxy::ProxyT>, Box<dyn pw::proxy::Listener>)>>>,
    mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
    _registry: pw::registry::RegistryRc,
    inventory: Rc<RefCell<Inventory>>,
}

impl Connection {
    fn new() -> Result<Self> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None).context("Connecting to PipeWire")?;
        let registry = core.get_registry_rc()?;
        let inventory = Rc::new(RefCell::new(Inventory::default()));
        let proxies = Rc::new(RefCell::new(Vec::new()));
        let weak = registry.downgrade();
        let state = inventory.clone();
        let held = proxies.clone();
        let removed = inventory.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                let Some(props) = global.props else { return };
                if global.type_ == pw::types::ObjectType::Port {
                    if let Some(channel) = props.get("audio.channel") {
                        let mut state = state.borrow_mut();
                        state.ports.insert(global.id, channel.to_owned());
                        state.revision += 1;
                    }
                } else if global.type_ == pw::types::ObjectType::Link {
                    let id = |key| props.get(key).and_then(|value| value.parse::<u32>().ok());
                    if let (
                        Some(output_node),
                        Some(output_port),
                        Some(input_node),
                        Some(input_port),
                    ) = (
                        id("link.output.node"),
                        id("link.output.port"),
                        id("link.input.node"),
                        id("link.input.port"),
                    ) {
                        let mut state = state.borrow_mut();
                        state.links.insert(
                            global.id,
                            Route {
                                output_node,
                                output_port,
                                input_node,
                                input_port,
                            },
                        );
                        state.revision += 1;
                    }
                }
                if global.type_ == pw::types::ObjectType::Node
                    && props.get("media.class") == Some("Audio/Sink")
                {
                    let (Some(name), Some(serial)) =
                        (props.get("node.name"), props.get("object.serial"))
                    else {
                        return;
                    };
                    let id = global.id;
                    state.borrow_mut().sinks.insert(
                        id,
                        Sink {
                            id,
                            serial: serial.to_owned(),
                            name: name.to_owned(),
                            positions: props.get("audio.position").unwrap_or("").to_owned(),
                        },
                    );
                    if let Ok(node) = registry.bind::<pw::node::Node, _>(global) {
                        let state = state.clone();
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                if let Some(props) = info.props() {
                                    if let Some(sink) = state.borrow_mut().sinks.get_mut(&id) {
                                        if let Some(positions) = props.get("audio.position") {
                                            sink.positions = positions.to_owned();
                                        }
                                    }
                                }
                            })
                            .register();
                        held.borrow_mut().push((
                            Box::new(node) as Box<dyn pw::proxy::ProxyT>,
                            Box::new(listener) as Box<dyn pw::proxy::Listener>,
                        ));
                    }
                } else if global.type_ == pw::types::ObjectType::Metadata
                    && props.get("metadata.name") == Some("default")
                {
                    if let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) {
                        let state = state.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |subject, key, _, value| {
                                if subject == 0 && key == Some("default.audio.sink") {
                                    state.borrow_mut().default = value
                                        .and_then(|value| {
                                            serde_json::from_str::<serde_json::Value>(value).ok()
                                        })
                                        .and_then(|value| {
                                            value
                                                .get("name")
                                                .and_then(|name| name.as_str())
                                                .map(str::to_owned)
                                        });
                                }
                                0
                            })
                            .register();
                        held.borrow_mut().push((
                            Box::new(metadata) as Box<dyn pw::proxy::ProxyT>,
                            Box::new(listener) as Box<dyn pw::proxy::Listener>,
                        ));
                    }
                }
            })
            .global_remove(move |id| {
                let mut state = removed.borrow_mut();
                state.sinks.remove(&id);
                state.ports.remove(&id);
                state.links.remove(&id);
                state.revision += 1;
            })
            .register();
        let connection = Self {
            mainloop,
            _context: context,
            core,
            _registry: registry,
            _listener: listener,
            _proxies: proxies,
            inventory,
        };
        // First sync discovers globals; second receives bound node properties
        // and default metadata. No output is opened during device enumeration.
        connection.roundtrip()?;
        connection.roundtrip()?;
        Ok(connection)
    }

    fn roundtrip(&self) -> Result<()> {
        let done = Rc::new(Cell::new(false));
        let completed = done.clone();
        let sequence = self.core.sync(0)?;
        let error = Rc::new(RefCell::new(None));
        let failed = error.clone();
        let _listener = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == sequence {
                    completed.set(true);
                }
            })
            .error(move |_, _, _, message| {
                *failed.borrow_mut() = Some(message.to_owned());
            })
            .register();
        let deadline = Instant::now() + TIMEOUT;
        while !done.get() {
            if let Some(message) = error.borrow_mut().take() {
                bail!("PipeWire: {message}");
            }
            if Instant::now() >= deadline {
                bail!("Timed out querying PipeWire devices");
            }
            self.mainloop.loop_().iterate(Duration::from_millis(10));
        }
        Ok(())
    }

    fn target(&self, name: Option<&str>) -> Result<Sink> {
        let inventory = self.inventory.borrow();
        let name = name
            .or(inventory.default.as_deref())
            .context("PipeWire has no default audio sink; select an explicit PipeWire device")?;
        inventory
            .sinks
            .values()
            .find(|sink| sink.name == name)
            .cloned()
            .with_context(|| format!("PipeWire audio sink not found: {name}"))
    }
}

pub(super) fn devices() -> Result<Vec<String>> {
    let connection = Connection::new()?;
    Ok(connection
        .inventory
        .borrow()
        .sinks
        .values()
        .map(|sink| format!("{PREFIX}{}", sink.name))
        .collect())
}

pub(super) fn validate(name: &str) -> Result<()> {
    Connection::new()?.target(Some(name)).map(|_| ())
}

pub(super) struct NativeStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl NativeStream {
    pub(super) fn open(
        name: Option<&str>,
        source_layout: &[Channel],
        callback: OutputCallback,
        errors: crossbeam_channel::Sender<cpal::Error>,
    ) -> Result<Self> {
        let name = name.map(str::to_owned);
        let source_layout = source_layout.to_vec();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (ready, receive) = crossbeam_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("rivu-pipewire".into())
            .spawn(move || {
                if let Err(error) =
                    run(name.as_deref(), &source_layout, callback, &stopping, &ready)
                {
                    let message = format!("{error:#}");
                    let _ = ready.try_send(Err(message.clone()));
                    let _ = errors.try_send(cpal::Error::with_message(
                        cpal::ErrorKind::BackendError,
                        message,
                    ));
                }
            })?;
        let stream = Self {
            stop,
            thread: Some(thread),
        };
        receive
            .recv_timeout(TIMEOUT * 4)
            .context("Timed out opening PipeWire output")?
            .map_err(anyhow::Error::msg)?;
        Ok(stream)
    }
}

fn run(
    name: Option<&str>,
    source_layout: &[Channel],
    mut callback: OutputCallback,
    stop: &AtomicBool,
    ready: &crossbeam_channel::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let connection = Connection::new()?;
    let target = connection.target(name)?;
    let speakers = parse_positions(&target.positions).with_context(|| {
        format!(
            "PipeWire sink {} has no usable speaker layout ({:?})",
            target.name, target.positions
        )
    })?;
    callback.mapping = layout::mapping(source_layout, &speakers)
        .with_context(|| format!("PipeWire sink {} cannot preserve this track", target.name))?;
    callback.channels = speakers.len();
    let rate = callback.rate;
    let mono = speakers == [Channel::FrontCenter] && target.positions.contains("MONO");
    let names: Vec<_> = speakers
        .iter()
        .map(|channel| {
            if mono {
                "MONO"
            } else {
                position_name(*channel)
            }
        })
        .collect();
    let positions = names.join(" ");
    let stream = pw::stream::StreamBox::new(
        &connection.core,
        "Rivu native audio",
        properties! {
            "media.type" => "Audio", "media.category" => "Playback", "media.role" => "Music",
            "application.name" => "Rivu", "node.name" => "rivu-native-output",
            "target.object" => target.serial.as_str(), "node.dont-reconnect" => "true",
            "node.dont-fallback" => "true", "stream.dont-remix" => "true",
            "channelmix.disable" => "true", "audio.channels" => speakers.len().to_string(),
            "audio.position" => format!("[ {positions} ]"), "audio.rate" => rate.to_string(),
        },
    )?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(speakers.len() as u32);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    for (position, speaker) in positions.iter_mut().zip(&speakers) {
        *position = spa_position(*speaker);
    }
    if mono {
        positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    }
    info.set_position(positions);
    let expected = info;
    let format_ok = Rc::new(Cell::new(false));
    let checked = format_ok.clone();
    let format_error = Rc::new(Cell::new(false));
    let processing = format_ok.clone();
    let rejected = format_error.clone();
    let graph = connection.inventory.clone();
    let route_ok = Rc::new(Cell::new(false));
    let routed = route_ok.clone();
    let expected_ports = Rc::new(names);
    let processing_ports = expected_ports.clone();
    let target_id = target.id;
    let mut revision = None;
    let _listener = stream
        .add_local_listener_with_user_data(callback)
        .param_changed(move |_, _, id, pod| {
            if id != spa::sys::SPA_PARAM_Format {
                return;
            }
            checked.set(false);
            if let Some(pod) = pod {
                let mut actual = spa::param::audio::AudioInfoRaw::new();
                let valid = actual.parse(pod).is_ok()
                    && actual.format() == expected.format()
                    && actual.rate() == expected.rate()
                    && actual.channels() == expected.channels()
                    && actual.position()[..actual.channels() as usize]
                        == expected.position()[..expected.channels() as usize];
                checked.set(valid);
                rejected.set(!valid);
            }
        })
        .process(move |stream, callback| {
            // Process runs on this dedicated loop, never on the UI/decoder thread.
            // All sample, mapping, analysis and clock storage was preallocated.
            let graph = graph.borrow();
            if revision != Some(graph.revision) {
                routed.set(route_matches(
                    &graph,
                    stream.node_id(),
                    target_id,
                    &processing_ports,
                ));
                revision = Some(graph.revision);
            }
            if processing.get() {
                unsafe {
                    process(stream, callback, routed.get());
                }
            }
        })
        .register()?;
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|error| anyhow::anyhow!("Serializing PipeWire audio format: {error:?}"))?
    .0
    .into_inner();
    stream.connect(
        spa::utils::Direction::Output,
        Some(target.id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&bytes).context("Invalid PipeWire format pod")?],
    )?;
    let deadline = Instant::now() + TIMEOUT;
    let mut started = false;
    let mut observed_revision = None;
    let failure = Rc::new(RefCell::new(None));
    let failed = failure.clone();
    let _core_listener = connection
        .core
        .add_listener_local()
        .error(move |_, _, _, message| {
            *failed.borrow_mut() = Some(message.to_owned());
        })
        .register();
    while !stop.load(Ordering::Acquire) {
        connection
            .mainloop
            .loop_()
            .iterate(Duration::from_millis(10));
        {
            let graph = connection.inventory.borrow();
            if observed_revision != Some(graph.revision) {
                route_ok.set(route_matches(
                    &graph,
                    stream.node_id(),
                    target.id,
                    &expected_ports,
                ));
                observed_revision = Some(graph.revision);
            }
        }
        if let Some(error) = failure.borrow_mut().take() {
            bail!("PipeWire output: {error}");
        }
        if format_error.get() {
            bail!("PipeWire changed the requested sample format, rate or speaker positions");
        }
        if connection.inventory.borrow().sinks.get(&target.id) != Some(&target) {
            bail!(
                "PipeWire target {} disappeared or changed its speaker layout; refusing to reroute",
                target.name
            );
        }
        if started && !route_ok.get() {
            bail!(
                "PipeWire speaker links changed or disappeared; refusing a partial/remixed route"
            );
        }
        match stream.state() {
            pw::stream::StreamState::Error(error) => bail!("PipeWire output: {error}"),
            pw::stream::StreamState::Unconnected if started => {
                bail!("PipeWire output disconnected")
            }
            pw::stream::StreamState::Streaming if format_ok.get() && route_ok.get() => {
                if !started {
                    let _ = ready.send(Ok(()));
                    started = true;
                }
            }
            _ if !started && Instant::now() >= deadline => bail!(
                "PipeWire sink {} did not accept/link the requested speaker layout",
                target.name
            ),
            _ => {}
        }
    }
    stream.disconnect()?;
    Ok(())
}

// SAFETY: invoked only by the owning PipeWire loop while the stream is alive.
// Mapped SPA buffers are checked for shape/alignment before making an f32 slice;
// each dequeued buffer is returned exactly once, including all failure paths.
unsafe fn process(stream: &pw::stream::Stream, callback: &mut OutputCallback, routed: bool) {
    unsafe {
        let raw = stream.as_raw_ptr();
        let buffer = pw::sys::pw_stream_dequeue_buffer(raw);
        if buffer.is_null() {
            return;
        }
        let result = render_buffer(raw, buffer, callback, routed);
        if !result {
            callback.shared.timing_error.store(3, Ordering::Release);
        }
        pw::sys::pw_stream_queue_buffer(raw, buffer);
    }
}

unsafe fn render_buffer(
    stream: *mut pw::sys::pw_stream,
    buffer: *mut pw::sys::pw_buffer,
    callback: &mut OutputCallback,
    routed: bool,
) -> bool {
    unsafe {
        (*buffer).size = 0;
        let spa_buffer = (*buffer).buffer;
        if spa_buffer.is_null() || (*spa_buffer).n_datas != 1 || (*spa_buffer).datas.is_null() {
            return false;
        }
        let data = &mut *(*spa_buffer).datas;
        if data.chunk.is_null() {
            return false;
        }
        (*data.chunk).offset = 0;
        (*data.chunk).size = 0;
        let stride = callback.channels * std::mem::size_of::<f32>();
        if data.data.is_null() || !(data.data as usize).is_multiple_of(std::mem::align_of::<f32>())
        {
            return false;
        }
        let maximum = data.maxsize as usize / stride;
        let requested = (*buffer).requested as usize;
        let frames = if requested == 0 {
            maximum
        } else {
            maximum.min(requested)
        };
        let samples =
            std::slice::from_raw_parts_mut(data.data.cast::<f32>(), frames * callback.channels);
        samples.fill(0.0);
        let mut time = std::mem::zeroed::<pw::sys::pw_time>();
        if pw::sys::pw_stream_get_time_n(stream, &mut time, std::mem::size_of::<pw::sys::pw_time>())
            < 0
        {
            return false;
        }
        let Some(timestamp) = output_timestamp(&time, callback.rate) else {
            return false;
        };
        if routed {
            callback.render(samples, timestamp);
        }
        (*data.chunk).stride = stride as i32;
        (*data.chunk).size = (frames * stride) as u32;
        (*buffer).size = frames as u64;
        true
    }
}

fn output_timestamp(time: &pw::sys::pw_time, rate: u32) -> Option<cpal::OutputStreamTimestamp> {
    if time.now < 0 || time.rate.num == 0 || time.rate.denom == 0 || rate == 0 {
        return None;
    }
    let now = time.now as u64;
    // PipeWire delay uses graph-rate ticks; queued/buffered use stream frames,
    // not samples or bytes. The pw_buffer.size we publish uses those same units.
    let delay = time.delay.max(0) as u128 * u128::from(time.rate.num) * 1_000_000_000
        / u128::from(time.rate.denom)
        + (u128::from(time.queued) + u128::from(time.buffered)) * 1_000_000_000 / u128::from(rate);
    let playback = now.checked_add(u64::try_from(delay).ok()?)?;
    Some(cpal::OutputStreamTimestamp {
        callback: cpal::StreamInstant::from_nanos(now),
        playback: cpal::StreamInstant::from_nanos(playback),
    })
}

// An accepted format alone is insufficient: session policy could leave LFE or
// RC unlinked, or connect a stream to a different sink. Require one direct link
// per speaker with equal labels on both ends, and no extra outgoing links.
fn route_matches(graph: &Inventory, source: u32, target: u32, speakers: &[&str]) -> bool {
    let routes = || {
        graph
            .links
            .values()
            .filter(|route| route.output_node == source)
    };
    if routes().count() != speakers.len() {
        return false;
    }
    speakers.iter().all(|speaker| {
        routes()
            .filter(|route| {
                route.input_node == target
                    && graph.ports.get(&route.output_port).map(String::as_str) == Some(*speaker)
                    && graph.ports.get(&route.input_port).map(String::as_str) == Some(*speaker)
            })
            .count()
            == 1
    })
}

fn parse_positions(text: &str) -> Result<Vec<Channel>> {
    let text = text.trim().trim_start_matches('[').trim_end_matches(']');
    let positions: Vec<_> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            let name = name.trim_matches('"');
            if name == "MONO" {
                return Ok(Channel::FrontCenter);
            }
            all_channels()
                .iter()
                .copied()
                .find(|channel| position_name(*channel) == name)
                .with_context(|| format!("Unsupported or unspecified PipeWire speaker {name:?}"))
        })
        .collect::<Result<_>>()?;
    layout::mapping(&positions, &positions)?;
    Ok(positions)
}

fn all_channels() -> &'static [Channel] {
    use Channel::*;
    &[
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        RearLeft,
        RearRight,
        RearCenter,
        SideLeft,
        SideRight,
        FrontLeftCenter,
        FrontRightCenter,
        TopCenter,
        TopFrontLeft,
        TopFrontCenter,
        TopFrontRight,
        TopRearLeft,
        TopRearCenter,
        TopRearRight,
    ]
}

fn position_name(channel: Channel) -> &'static str {
    use Channel::*;
    match channel {
        FrontLeft => "FL",
        FrontRight => "FR",
        FrontCenter => "FC",
        Lfe => "LFE",
        RearLeft => "RL",
        RearRight => "RR",
        RearCenter => "RC",
        SideLeft => "SL",
        SideRight => "SR",
        FrontLeftCenter => "FLC",
        FrontRightCenter => "FRC",
        TopCenter => "TC",
        TopFrontLeft => "TFL",
        TopFrontCenter => "TFC",
        TopFrontRight => "TFR",
        TopRearLeft => "TRL",
        TopRearCenter => "TRC",
        TopRearRight => "TRR",
    }
}

fn spa_position(channel: Channel) -> u32 {
    use Channel::*;
    use spa::sys::*;
    match channel {
        FrontLeft => SPA_AUDIO_CHANNEL_FL,
        FrontRight => SPA_AUDIO_CHANNEL_FR,
        FrontCenter => SPA_AUDIO_CHANNEL_FC,
        Lfe => SPA_AUDIO_CHANNEL_LFE,
        RearLeft => SPA_AUDIO_CHANNEL_RL,
        RearRight => SPA_AUDIO_CHANNEL_RR,
        RearCenter => SPA_AUDIO_CHANNEL_RC,
        SideLeft => SPA_AUDIO_CHANNEL_SL,
        SideRight => SPA_AUDIO_CHANNEL_SR,
        FrontLeftCenter => SPA_AUDIO_CHANNEL_FLC,
        FrontRightCenter => SPA_AUDIO_CHANNEL_FRC,
        TopCenter => SPA_AUDIO_CHANNEL_TC,
        TopFrontLeft => SPA_AUDIO_CHANNEL_TFL,
        TopFrontCenter => SPA_AUDIO_CHANNEL_TFC,
        TopFrontRight => SPA_AUDIO_CHANNEL_TFR,
        TopRearLeft => SPA_AUDIO_CHANNEL_TRL,
        TopRearCenter => SPA_AUDIO_CHANNEL_TRC,
        TopRearRight => SPA_AUDIO_CHANNEL_TRR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::*;

    #[test]
    fn explicit_sink_positions_preserve_rear_center_and_reject_unknowns() {
        assert_eq!(
            parse_positions("[ FL FR FC LFE RC SL SR ]").unwrap(),
            [
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight
            ]
        );
        assert_eq!(
            parse_positions("[\"FL\",\"FR\",\"RL\",\"RR\",\"FC\",\"LFE\"]").unwrap(),
            [FrontLeft, FrontRight, RearLeft, RearRight, FrontCenter, Lfe]
        );
        assert_eq!(parse_positions("[ MONO ]").unwrap(), [FrontCenter]);
        for invalid in [
            "",
            "[]",
            "[ FL FR UNKNOWN ]",
            "[ FL FR AUX0 ]",
            "[ FL FR FL ]",
        ] {
            assert!(parse_positions(invalid).is_err(), "{invalid}");
        }
        for channel in all_channels() {
            assert_eq!(
                parse_positions(position_name(*channel)).unwrap(),
                [*channel]
            );
        }
    }

    #[test]
    fn actual_links_must_include_lfe_and_rear_center_without_cross_routing() {
        let speakers = ["FL", "FR", "FC", "LFE", "RC", "SL", "SR"];
        let mut graph = Inventory::default();
        for (index, speaker) in speakers.iter().enumerate() {
            let index = index as u32;
            graph.ports.insert(100 + index, (*speaker).to_owned());
            graph.ports.insert(200 + index, (*speaker).to_owned());
            graph.links.insert(
                index,
                Route {
                    output_node: 1,
                    output_port: 100 + index,
                    input_node: 2,
                    input_port: 200 + index,
                },
            );
        }
        assert!(route_matches(&graph, 1, 2, &speakers));
        graph.ports.insert(203, "FL".into());
        assert!(!route_matches(&graph, 1, 2, &speakers));
        graph.ports.insert(203, "LFE".into());
        graph.links.get_mut(&4).unwrap().input_node = 3;
        assert!(!route_matches(&graph, 1, 2, &speakers));
        graph.links.get_mut(&4).unwrap().input_node = 2;
        graph.links.remove(&4);
        assert!(!route_matches(&graph, 1, 2, &speakers));
    }

    #[test]
    fn native_clock_uses_graph_ticks_and_stream_frames_not_channel_samples() {
        let mut time = unsafe { std::mem::zeroed::<pw::sys::pw_time>() };
        time.now = 2_000_000_000;
        time.rate.num = 1;
        time.rate.denom = 48_000;
        time.delay = 480;
        time.queued = 8;
        time.buffered = 16;
        let timestamp = output_timestamp(&time, 48_000).unwrap();
        assert_eq!(
            timestamp.playback.duration_since(timestamp.callback),
            Duration::from_micros(10_500)
        );
        time.delay = -480;
        let timestamp = output_timestamp(&time, 48_000).unwrap();
        assert_eq!(
            timestamp.playback.duration_since(timestamp.callback),
            Duration::from_micros(500)
        );
        time.rate.denom = 0;
        assert!(output_timestamp(&time, 48_000).is_none());
        time.rate.denom = 48_000;
        time.now = -1;
        assert!(output_timestamp(&time, 48_000).is_none());
    }
}

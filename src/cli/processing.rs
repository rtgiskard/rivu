use super::args::{
    ProcessingAction, ProcessingAudition, ProcessingGraphAction, ProcessingGraphMode,
    ProcessingKind,
};
use anyhow::{Context, Result};
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{model::*, response::StateSections};
use std::path::Path;

fn show_settings(
    i18n: &Localizer,
    config: &rivu::config::Config,
    task: Option<&ProcessingTaskStatus>,
    instance_id: u32,
    graph_only: bool,
) -> Result<()> {
    if !graph_only {
        println!(
            "{}",
            i18n.format(
                "core-processing-equalizer",
                &[
                    (
                        "enabled",
                        i18n.text(if config.equalizer.enabled {
                            "core-yes"
                        } else {
                            "core-no"
                        })
                        .into()
                    ),
                    ("preamp", config.equalizer.preamp_db.to_string().into()),
                    ("bands", config.equalizer.bands.len().to_string().into()),
                ]
            )
        );
        let mode = match config.denoise.mode {
            rivu_dsp::denoise::DenoiseMode::Off => "processing-off",
            rivu_dsp::denoise::DenoiseMode::Stationary => "processing-stationary",
            rivu_dsp::denoise::DenoiseMode::Profile => "processing-profile",
            rivu_dsp::denoise::DenoiseMode::Adaptive => "processing-adaptive",
        };
        println!(
            "{}",
            i18n.format(
                "core-processing-denoise",
                &[
                    (
                        "enabled",
                        i18n.text(if !config.denoise.is_bypassed() {
                            "core-yes"
                        } else {
                            "core-no"
                        })
                        .into()
                    ),
                    ("mode", i18n.text(mode).into()),
                ]
            )
        );
        println!(
            "{}",
            i18n.format(
                "core-processing-neural",
                &[
                    (
                        "device",
                        match config.neural.device {
                            rivu::config::NeuralDevice::Cpu => "cpu",
                            rivu::config::NeuralDevice::Cuda => "cuda",
                        }
                        .into()
                    ),
                    ("threads", config.neural.threads.to_string().into()),
                ]
            )
        );
    }
    let graph = &config.processing_graph;
    println!(
        "{}",
        i18n.format(
            "core-processing-graph",
            &[
                ("revision", graph.revision.to_string().into()),
                ("nodes", graph.nodes.len().to_string().into()),
                ("edges", graph.edges.len().to_string().into()),
            ]
        )
    );
    if graph_only {
        for node in &graph.nodes {
            let kind = match node.kind {
                GraphNodeKind::Source => "processing-source",
                GraphNodeKind::Equalizer { .. } => "processing-equalizer",
                GraphNodeKind::Denoise { .. } => "processing-restoration",
                GraphNodeKind::Speech { .. } => "processing-speech",
                GraphNodeKind::Separate { .. } => "processing-separation",
                GraphNodeKind::Mix { .. } => "processing-mix",
                GraphNodeKind::Output => "processing-graph-output",
            };
            println!(
                "{}",
                i18n.format(
                    "core-processing-node",
                    &[
                        ("id", node.id.to_string().into()),
                        ("kind", i18n.text(kind).into()),
                    ]
                )
            );
        }
        for edge in &graph.edges {
            println!(
                "{}:{} -> {}:{}",
                edge.from.node, edge.from.port, edge.to.node, edge.to.port
            );
        }
    }
    if let Some(task) = task {
        super::tasks::emit_processing(i18n, instance_id, task, false)?;
    } else {
        println!("{}", i18n.text("core-processing-latest"));
    }
    Ok(())
}

fn equalizer_command(edit: &str) -> Result<Command> {
    let mut edit: EqualizerEdit =
        serde_json::from_str(edit).context("Parsing EqualizerEdit JSON")?;
    match &mut edit {
        EqualizerEdit::ImportPreset { path, .. } | EqualizerEdit::ExportPreset { path, .. } => {
            *path = super::client_path(std::mem::take(path))?;
        }
        _ => {}
    }
    Ok(Command::EqualizerEdit { edit })
}

fn show_processing(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    json: bool,
    graph_only: bool,
) -> Result<()> {
    let response = session.get_state(StateSections {
        processing: true,
        configuration: true,
        ..Default::default()
    })?;
    let state = response.processing.context("Missing processing state")?;
    let config = response
        .configuration
        .context("Missing configuration state")?
        .config;
    let instance_id = super::tasks::instance_id(session)?;
    if json {
        let processing = state
            .task
            .as_ref()
            .map(|task| super::tasks::task_json(instance_id, task.id, task))
            .transpose()?;
        let value = if graph_only {
            serde_json::json!({"graph":config.processing_graph,"latest_task":processing})
        } else {
            serde_json::json!({"equalizer":config.equalizer,"presets":config.equalizer_presets,"equalizer_ab":config.equalizer_ab,"denoise":config.denoise,"neural":config.neural,"processing_graph":config.processing_graph,"processing":processing})
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        show_settings(
            i18n,
            &config,
            state.task.as_deref(),
            instance_id,
            graph_only,
        )?;
    }
    Ok(())
}

pub(super) fn run(
    i18n: &Localizer,
    data_dir: &Path,
    action: ProcessingAction,
    json: bool,
) -> Result<()> {
    match action {
        ProcessingAction::Setup(setup) => super::processing_setup::run(data_dir, setup, json),
        action => run_live(i18n, data_dir, action, json),
    }
}

fn run_live(i18n: &Localizer, data_dir: &Path, action: ProcessingAction, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let mut wait = false;
    let command = match action {
        ProcessingAction::Show => return show_processing(i18n, &mut session, json, false),
        ProcessingAction::Setup(_) => unreachable!("setup does not use a Core session"),
        ProcessingAction::Bypass => Command::ProcessingBypass,
        ProcessingAction::Graph(action) => match action {
            ProcessingGraphAction::Show => {
                return show_processing(i18n, &mut session, json, true);
            }
            ProcessingGraphAction::Edit {
                edit,
                expected_revision,
            } => Command::ProcessingGraphEdit {
                expected_revision,
                edit: serde_json::from_str(&edit).context("Parsing GraphEdit JSON")?,
            },
            ProcessingGraphAction::Run {
                track_id,
                node,
                port,
                mode,
                wait: requested,
            } => {
                wait = requested;
                Command::ProcessingGraphRun {
                    track_id,
                    target: GraphPort { node, port },
                    mode: match mode {
                        ProcessingGraphMode::Auto => GraphExecutionMode::Auto,
                        ProcessingGraphMode::Offline => GraphExecutionMode::Offline,
                        ProcessingGraphMode::Realtime => GraphExecutionMode::Realtime,
                        ProcessingGraphMode::Hybrid => GraphExecutionMode::Hybrid,
                    },
                }
            }
            ProcessingGraphAction::Audition { id, node, port } => {
                session.bind_instance(id.instance_id)?;
                Command::ProcessingGraphAudition {
                    id: id.id,
                    node: GraphPort { node, port },
                }
            }
            ProcessingGraphAction::Export {
                id,
                node,
                port,
                path,
                wait: requested,
            } => {
                session.bind_instance(id.instance_id)?;
                wait = requested;
                Command::ProcessingGraphExport {
                    id: id.id,
                    node: GraphPort { node, port },
                    path: super::client_path(path)?,
                }
            }
        },
        ProcessingAction::Eq { edit } => equalizer_command(&edit)?,
        ProcessingAction::Denoise { patch } => Command::DenoiseEdit {
            patch: serde_json::from_str(&patch).context("Parsing DenoisePatch JSON")?,
        },
        ProcessingAction::Neural { patch } => Command::NeuralEdit {
            patch: serde_json::from_str(&patch).context("Parsing NeuralPatch JSON")?,
        },
        ProcessingAction::Start {
            track_id,
            operation,
            wait: requested,
            noise_start,
            noise_end,
        } => {
            wait = requested;
            let operation = match operation {
                ProcessingKind::Denoise => ProcessingOperation::Denoise,
                ProcessingKind::Speech => ProcessingOperation::Speech,
                ProcessingKind::Separate => ProcessingOperation::Separate,
            };
            let profile = noise_start
                .zip(noise_end)
                .map(|(start_seconds, end_seconds)| NoiseSelection {
                    start_seconds,
                    end_seconds,
                });
            Command::StartProcessing {
                track_id,
                operation,
                profile,
            }
        }
        ProcessingAction::Audition {
            id,
            mode,
            vocal_gain_db,
            accompaniment_gain_db,
        } => {
            session.bind_instance(id.instance_id)?;
            Command::AuditionProcessing {
                id: id.id,
                mode: match mode {
                    ProcessingAudition::Original => AuditionMode::Original,
                    ProcessingAudition::Denoised => AuditionMode::Denoised,
                    ProcessingAudition::Vocals => AuditionMode::Vocals,
                    ProcessingAudition::Accompaniment => AuditionMode::Accompaniment,
                    ProcessingAudition::Mix => AuditionMode::Mix,
                },
                vocal_gain_db,
                accompaniment_gain_db,
            }
        }
    };
    let ack = session.request_ack(command)?;
    if !wait || !ack.is_success() {
        return super::output::show_task_ack(i18n, &session, &ack, json);
    }
    let id = ack
        .processing_id
        .context("Processing startup omitted task id")?;
    super::tasks::wait_processing(i18n, &mut session, id, json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Action, Args};
    use clap::Parser;
    use std::path::PathBuf;

    fn parse_graph_action(arguments: &[&str]) -> ProcessingGraphAction {
        let args = Args::try_parse_from(
            ["rivu", "processing", "graph"]
                .into_iter()
                .chain(arguments.iter().copied()),
        )
        .unwrap();
        let Some(Action::Processing(ProcessingAction::Graph(action))) = args.action else {
            panic!("expected graph action");
        };
        action
    }

    #[test]
    fn retained_graph_outputs_preserve_instance_reference_and_client_path() {
        let ProcessingGraphAction::Audition { id, node, port } =
            parse_graph_action(&["audition", "8:91", "7", "--port", "1"])
        else {
            panic!("expected graph audition action");
        };
        assert_eq!(id.instance_id, 8);
        assert_eq!(id.id, 91);
        assert_eq!(node, 7);
        assert_eq!(port, 1);

        let relative = "rendered outputs/voice -6 dB.wav";
        let absolute = std::env::current_dir().unwrap().join(relative);
        for path in [PathBuf::from(relative), absolute.clone()] {
            for requested_wait in [false, true] {
                let mut arguments =
                    vec!["export", "8:91", "7", path.to_str().unwrap(), "--port", "1"];
                if requested_wait {
                    arguments.push("--wait");
                }
                let ProcessingGraphAction::Export {
                    id,
                    node,
                    port,
                    path,
                    wait,
                } = parse_graph_action(&arguments)
                else {
                    panic!("expected graph export action");
                };
                assert_eq!(id.instance_id, 8);
                assert_eq!(id.id, 91);
                assert_eq!(node, 7);
                assert_eq!(port, 1);
                assert_eq!(super::super::client_path(path).unwrap(), absolute);
                assert_eq!(wait, requested_wait);
            }
        }
    }
}

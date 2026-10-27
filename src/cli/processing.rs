use super::args::{
    ProcessingAction, ProcessingAudition, ProcessingGraphAction, ProcessingGraphMode,
    ProcessingKind,
};
use anyhow::{Context, Result};
use rivu::i18n::Localizer;
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
                        i18n.text(
                            if config.denoise.wet > 0.0
                                && (config.denoise.mode != rivu_dsp::denoise::DenoiseMode::Off
                                    || config.denoise.hum.enabled
                                    || config.denoise.impulse.enabled)
                            {
                                "core-yes"
                            } else {
                                "core-no"
                            }
                        )
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

fn graph_command(action: ProcessingGraphAction) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
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
        ProcessingGraphAction::Audition { id, node, port } => Command::ProcessingGraphAudition {
            id: id.id,
            node: GraphPort { node, port },
        },
        ProcessingGraphAction::Export {
            id,
            node,
            port,
            path,
            wait: requested,
        } => {
            wait = requested;
            Command::ProcessingGraphExport {
                id: id.id,
                node: GraphPort { node, port },
                path: super::client_path(path)?,
            }
        }
        ProcessingGraphAction::Show => unreachable!("Graph show is a state query"),
    };
    Ok((command, wait))
}

pub(super) fn run(
    i18n: &Localizer,
    data_dir: &Path,
    action: ProcessingAction,
    json: bool,
) -> Result<()> {
    let graph_show = matches!(
        &action,
        ProcessingAction::Graph(ProcessingGraphAction::Show)
    );
    if graph_show || matches!(&action, ProcessingAction::Show) {
        let mut session = rivu_client::client_session(data_dir)?;
        let state = session
            .get_state(StateSections {
                system: true,
                ..Default::default()
            })?
            .system
            .context("Missing system state")?;
        let instance_id = super::tasks::instance_id(&session)?;
        if json {
            let processing = state
                .processing
                .as_ref()
                .map(|task| super::tasks::task_json(instance_id, task.id, task))
                .transpose()?;
            let value = if graph_show {
                serde_json::json!({"graph":state.config.processing_graph,"latest_task":processing})
            } else {
                serde_json::json!({"equalizer":state.config.equalizer,"presets":state.config.equalizer_presets,"equalizer_ab":state.config.equalizer_ab,"denoise":state.config.denoise,"neural":state.config.neural,"processing_graph":state.config.processing_graph,"processing":processing})
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            show_settings(
                i18n,
                &state.config,
                state.processing.as_ref(),
                instance_id,
                graph_show,
            )?;
        }
        return Ok(());
    }
    let mut session = rivu_client::client_session(data_dir)?;
    let reference = match &action {
        ProcessingAction::Audition { id, .. }
        | ProcessingAction::Graph(ProcessingGraphAction::Audition { id, .. })
        | ProcessingAction::Graph(ProcessingGraphAction::Export { id, .. }) => Some(*id),
        _ => None,
    };
    if let Some(reference) = reference {
        session.bind_instance(reference.instance_id)?;
    }
    let mut wait = false;
    let command = match action {
        ProcessingAction::Bypass => Command::ProcessingBypass,
        ProcessingAction::Graph(action) => {
            let (command, requested) = graph_command(action)?;
            wait = requested;
            command
        }
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
        } => Command::AuditionProcessing {
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
        },
        ProcessingAction::Show => unreachable!(),
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

    #[test]
    fn preset_paths_are_anchored_to_client_without_expansion_or_existence() {
        let cwd = std::env::current_dir().unwrap();
        for variant in ["ImportPreset", "ExportPreset"] {
            for input in [
                PathBuf::from("~/missing preset.json"),
                cwd.join("absent.json"),
            ] {
                let expected = if input.is_absolute() {
                    input.clone()
                } else {
                    cwd.join(&input)
                };
                let json = serde_json::json!({(variant):{"path":input,"name":"Reference"}});
                let Command::EqualizerEdit { edit } = equalizer_command(&json.to_string()).unwrap()
                else {
                    panic!("expected equalizer edit");
                };
                match edit {
                    EqualizerEdit::ImportPreset { path, name }
                    | EqualizerEdit::ExportPreset { path, name } => {
                        assert_eq!(path, expected);
                        assert_eq!(name, "Reference");
                    }
                    _ => panic!("expected preset edit"),
                }
            }
        }
    }

    fn parse_graph_command(arguments: &[&str]) -> (Command, bool) {
        let args = Args::try_parse_from(
            ["rivu", "processing", "graph"]
                .into_iter()
                .chain(arguments.iter().copied()),
        )
        .unwrap();
        let Some(Action::Processing(ProcessingAction::Graph(action))) = args.action else {
            panic!("expected graph action");
        };
        graph_command(action).unwrap()
    }

    #[test]
    fn graph_edit_parses_typed_json_and_preserves_revision_and_signed_gains() {
        let edit = r#"{"Replace":{"id":3,"kind":{"mix":{"gains_db":[-6.0,-12.5],"muted":[false,false],"solo":[false,false]}}}}"#;
        let (command, wait) = parse_graph_command(&["edit", edit, "--expected-revision", "19"]);
        let Command::ProcessingGraphEdit {
            expected_revision,
            edit,
        } = command
        else {
            panic!("expected graph edit command");
        };
        assert_eq!(expected_revision, 19);
        assert!(!wait);
        let GraphEdit::Replace {
            id,
            kind: GraphNodeKind::Mix { gains_db, .. },
        } = *edit
        else {
            panic!("expected mix replacement");
        };
        assert_eq!(id, 3);
        assert_eq!(gains_db, [-6.0, -12.5]);
        assert!(
            graph_command(ProcessingGraphAction::Edit {
                expected_revision: 19,
                edit: "not GraphEdit JSON".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn graph_run_preserves_target_mode_and_existing_wait_intent() {
        for (argument, expected) in [
            ("auto", GraphExecutionMode::Auto),
            ("offline", GraphExecutionMode::Offline),
            ("realtime", GraphExecutionMode::Realtime),
            ("hybrid", GraphExecutionMode::Hybrid),
        ] {
            let (command, wait) = parse_graph_command(&[
                "run", "42", "7", "--port", "1", "--mode", argument, "--wait",
            ]);
            let Command::ProcessingGraphRun {
                track_id,
                target,
                mode,
            } = command
            else {
                panic!("expected graph run command");
            };
            assert_eq!(track_id, 42);
            assert_eq!(target, GraphPort { node: 7, port: 1 });
            assert_eq!(mode, expected);
            assert!(wait);
        }
    }

    #[test]
    fn retained_graph_output_commands_preserve_run_port_and_client_path() {
        let (command, wait) = parse_graph_command(&["audition", "8:91", "7", "--port", "1"]);
        assert!(matches!(
            command,
            Command::ProcessingGraphAudition {
                id: 91,
                node: GraphPort { node: 7, port: 1 },
            }
        ));
        assert!(!wait);

        let relative = "rendered outputs/voice -6 dB.wav";
        let absolute = std::env::current_dir().unwrap().join(relative);
        for path in [PathBuf::from(relative), absolute.clone()] {
            for requested_wait in [false, true] {
                let mut arguments =
                    vec!["export", "8:91", "7", path.to_str().unwrap(), "--port", "1"];
                if requested_wait {
                    arguments.push("--wait");
                }
                let (command, wait) = parse_graph_command(&arguments);
                let Command::ProcessingGraphExport { id, node, path } = command else {
                    panic!("expected graph export command");
                };
                assert_eq!(id, 91);
                assert_eq!(node, GraphPort { node: 7, port: 1 });
                assert_eq!(path, absolute);
                assert_eq!(wait, requested_wait);
            }
        }
    }
}

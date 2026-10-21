use super::{
    args::{
        ProcessingAction, ProcessingAudition, ProcessingGraphAction, ProcessingGraphMode,
        ProcessingKind,
    },
    output::Terminal,
};
use anyhow::{Context, Result, bail};
use rivu::{
    ipc,
    model::*,
    response::{StateSections, ViewResponse},
};
use std::{path::Path, thread, time::Duration};

pub(super) fn run(data_dir: &Path, action: ProcessingAction, json: bool) -> Result<()> {
    let result = execute(data_dir, action, json);
    if let Err(error) = &result
        && json
    {
        println!(
            "{}",
            serde_json::json!({"type":"error","error":format!("{error:#}")})
        );
    }
    result
}
fn status(session: &mut ipc::ClientSession, id: u64) -> Result<ProcessingTaskStatus> {
    let view = session
        .query(&Query::ProcessingTask { id })?
        .result
        .map_err(anyhow::Error::from)?;
    let ViewResponse::ProcessingTask(task) = view else {
        bail!("Unexpected processing task response")
    };
    task.map(|task| *task)
        .context("Unknown or expired processing task")
}
fn emit(task: &ProcessingTaskStatus, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({"type":if task.phase.is_terminal(){"result"}else{"progress"},"task":task})
        );
    } else {
        println!(
            "{}\t{:?}\t{} / {}",
            task.id,
            task.phase,
            task.completed,
            task.total.map_or_else(|| "?".into(), |v| v.to_string())
        );
        if let Some(error) = &task.error {
            eprintln!("{}", Terminal(error));
        }
    }
    Ok(())
}

fn graph_command(action: ProcessingGraphAction) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
        ProcessingGraphAction::Edit { edit, revision } => Command::ProcessingGraphEdit {
            revision,
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
            id,
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
                id,
                node: GraphPort { node, port },
                path: if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .context("Resolving graph export path against client directory")?
                        .join(path)
                },
            }
        }
        ProcessingGraphAction::Show => unreachable!("Graph show is a state query"),
    };
    Ok((command, wait))
}

fn execute(data_dir: &Path, action: ProcessingAction, json: bool) -> Result<()> {
    let graph_show = matches!(
        &action,
        ProcessingAction::Graph(ProcessingGraphAction::Show)
    );
    if graph_show || matches!(&action, ProcessingAction::Show) {
        let state = ipc::get_state(
            data_dir,
            StateSections {
                system: true,
                ..Default::default()
            },
        )?
        .system
        .context("Missing system state")?;
        let value = if graph_show {
            serde_json::json!({"graph":state.config.processing_graph,"latest_task":state.processing})
        } else {
            serde_json::json!({"equalizer":state.config.equalizer,"presets":state.config.equalizer_presets,"equalizer_ab":state.config.equalizer_ab,"denoise":state.config.denoise,"neural":state.config.neural,"processing_graph":state.config.processing_graph,"processing":state.processing})
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let mut session = ipc::client_session(data_dir)?;
    let mut wait = false;
    let command = match action {
        ProcessingAction::Graph(action) => {
            let (command, requested) = graph_command(action)?;
            wait = requested;
            command
        }
        ProcessingAction::Eq { edit } => Command::EqualizerEdit {
            edit: serde_json::from_str(&edit).context("Parsing EqualizerEdit JSON")?,
        },
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
            let profile = match (noise_start, noise_end) {
                (Some(start_seconds), Some(end_seconds)) => Some(NoiseSelection {
                    start_seconds,
                    end_seconds,
                }),
                (None, None) => None,
                _ => bail!("Both --noise-start and --noise-end are required"),
            };
            Command::StartProcessing {
                track_id,
                operation,
                profile,
            }
        }
        ProcessingAction::Cancel { id } => Command::CancelProcessing { id },
        ProcessingAction::Status { id } => {
            let task = status(&mut session, id)?;
            return emit(&task, json);
        }
        ProcessingAction::Audition {
            id,
            mode,
            vocal_gain_db,
            accompaniment_gain_db,
        } => Command::AuditionProcessing {
            id,
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
    let ack = session.request_ack(&command)?;
    if !ack.is_success() {
        bail!("{}", ack.error.as_deref().unwrap_or("Command rejected"));
    }
    if json {
        println!("{}", serde_json::json!({"type":"accepted","ack":ack}));
    } else {
        println!(
            "Accepted{}",
            ack.processing_id
                .map_or_else(String::new, |id| format!(" task {id}"))
        );
    }
    if !wait {
        return Ok(());
    }
    let id = ack
        .processing_id
        .context("Processing startup omitted task id")?;
    let mut previous = None;
    loop {
        let task = status(&mut session, id)?;
        if previous.as_ref() != Some(&task) {
            emit(&task, json)?;
            previous = Some(task.clone());
        }
        match task.phase {
            ProcessingPhase::Complete => return Ok(()),
            ProcessingPhase::Cancelled | ProcessingPhase::Failed => bail!(
                "{}",
                task.error.as_deref().unwrap_or("Processing cancelled")
            ),
            _ => thread::sleep(Duration::from_millis(150)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Action, Args};
    use clap::Parser;
    use std::path::PathBuf;

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
        let (command, wait) = parse_graph_command(&["edit", edit, "--revision", "19"]);
        let Command::ProcessingGraphEdit { revision, edit } = command else {
            panic!("expected graph edit command");
        };
        assert_eq!(revision, 19);
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
                revision: 19,
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
        let (command, wait) = parse_graph_command(&["audition", "91", "7", "--port", "1"]);
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
                    vec!["export", "91", "7", path.to_str().unwrap(), "--port", "1"];
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

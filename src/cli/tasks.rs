use super::{
    args::{TaskAction, TaskKind},
    output::{OutputMode, show_scan},
};
use anyhow::{Context, Result, bail, ensure};
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{
    model::{
        Command, ProcessingPhase, ProcessingTaskStatus, Query, RelocationPhase,
        RelocationTaskStatus, ScanPhase, ScanTaskStatus, TaskTarget,
    },
    response::{TaskOutcome, ViewResponse},
};
use rivu_utils::terminal::TerminalEscaped;
use std::{io::Write, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TaskReference {
    pub(super) instance_id: u32,
    pub(super) id: u64,
}

impl std::str::FromStr for TaskReference {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (instance, id) = value.split_once(':').context("Expected INSTANCE:TASK_ID")?;
        ensure!(
            !instance.is_empty()
                && !id.is_empty()
                && instance.bytes().all(|byte| byte.is_ascii_digit())
                && id.bytes().all(|byte| byte.is_ascii_digit()),
            "Expected decimal INSTANCE:TASK_ID"
        );
        let reference = Self {
            instance_id: instance.parse()?,
            id: id.parse()?,
        };
        ensure!(
            reference.instance_id != 0 && reference.id != 0,
            "Task identity must be nonzero"
        );
        Ok(reference)
    }
}

impl std::fmt::Display for TaskReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.instance_id, self.id)
    }
}

pub(super) fn instance_id(session: &rivu_client::ClientSession) -> Result<u32> {
    session
        .instance_id()
        .context("Task response omitted instance identity")
}

pub(super) fn task_json<T: serde::Serialize>(
    instance_id: u32,
    id: u64,
    task: &T,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(task)?;
    let fields = value.as_object_mut().context("Expected task object")?;
    fields.insert("instance_id".into(), instance_id.into());
    fields.insert(
        "task_ref".into(),
        TaskReference { instance_id, id }.to_string().into(),
    );
    Ok(value)
}

pub(super) fn run(
    i18n: &Localizer,
    data_dir: &Path,
    action: TaskAction,
    mode: OutputMode,
) -> Result<()> {
    let json = mode == OutputMode::Json;
    let mut session = rivu_client::client_session(data_dir);
    let reference = match &action {
        TaskAction::Status { id, .. } | TaskAction::Cancel { id, .. } => *id,
    };
    session.bind_instance(reference.instance_id)?;
    let id = reference.id;
    match action {
        TaskAction::Status { kind, .. } => match kind {
            TaskKind::Scan => emit_scan(
                i18n,
                reference.instance_id,
                &scan_status(&mut session, id)?,
                json,
                None,
            ),
            TaskKind::Relocation => emit_relocation(
                i18n,
                reference.instance_id,
                &relocation_status(&mut session, id)?,
                json,
                true,
            ),
            TaskKind::Processing => emit_processing(
                i18n,
                reference.instance_id,
                &processing_status(&mut session, id)?,
                json,
                true,
            ),
        },
        TaskAction::Cancel { kind, .. } => {
            let command = match kind {
                TaskKind::Scan => Command::CancelScan { id },
                TaskKind::Relocation => Command::CancelRelocation { id },
                TaskKind::Processing => Command::CancelProcessing { id },
            };
            let ack = super::output::legacy_ack(session.request_command(command)?)?;
            super::output::show_task_ack(i18n, &session, &ack, mode)
        }
    }
}

fn processing_status(
    session: &mut rivu_client::ClientSession,
    id: u64,
) -> Result<ProcessingTaskStatus> {
    let view = session
        .query(Query::ProcessingTask { id })?
        .result
        .map_err(anyhow::Error::from)?;
    let ViewResponse::ProcessingTask(task) = view else {
        bail!("Unexpected processing task response")
    };
    let task = task
        .map(|task| *task)
        .context("Unknown or expired processing task")?;
    ensure!(
        task.id == id,
        "Processing query returned a different task ID"
    );
    Ok(task)
}
pub(super) fn emit_processing(
    i18n: &Localizer,
    instance_id: u32,
    task: &ProcessingTaskStatus,
    json: bool,
    show_error: bool,
) -> Result<()> {
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(&task_json(instance_id, task.id, task)?)?
        )?;
    } else {
        let phase = rivu_presentation::processing::phase_key(task.phase);
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-processing-task",
                &[
                    (
                        "id",
                        TaskReference {
                            instance_id,
                            id: task.id
                        }
                        .to_string()
                        .into()
                    ),
                    ("phase", i18n.text(phase).into()),
                    ("completed", task.completed.to_string().into()),
                    (
                        "total",
                        task.total
                            .map_or_else(|| "?".into(), |total| total.to_string())
                            .into()
                    ),
                ]
            )
        )?;
        if let Some(path) = &task.graph_exported_path {
            writeln!(
                output,
                "{}",
                i18n.format(
                    "core-processing-exported",
                    &[("path", TerminalEscaped(path.display()).to_string().into())]
                )
            )?;
        }
        if show_error && let Some(error) = &task.error {
            writeln!(output, "{}", TerminalEscaped(error))?;
        }
    }
    output.flush()?;
    Ok(())
}
fn scan_status(session: &mut rivu_client::ClientSession, id: u64) -> Result<ScanTaskStatus> {
    let view = session
        .query(Query::ScanTask { id })?
        .result
        .map_err(anyhow::Error::from)?;
    let ViewResponse::ScanTask(task) = view else {
        bail!("Scan task query returned an unexpected response");
    };
    if task.id != id {
        bail!("Scan task query returned a different task ID");
    }
    Ok(task)
}

fn relocation_status(
    session: &mut rivu_client::ClientSession,
    id: u64,
) -> Result<RelocationTaskStatus> {
    let view = session
        .query(Query::RelocationTask { id })?
        .result
        .map_err(anyhow::Error::from)?;
    let ViewResponse::RelocationTask(task) = view else {
        bail!("Relocation task query returned an unexpected response");
    };
    if task.id != id {
        bail!("Relocation task query returned a different task ID");
    }
    Ok(task)
}

fn emit_scan(
    i18n: &Localizer,
    instance_id: u32,
    task: &ScanTaskStatus,
    json: bool,
    reported_error: Option<&str>,
) -> Result<()> {
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(&task_json(instance_id, task.id, task)?)?
        )?;
    } else {
        writeln!(
            output,
            "{}",
            TaskReference {
                instance_id,
                id: task.id
            }
        )?;
        show_scan(i18n, &task.progress)?;
        write_scan_outcome(&mut output, i18n, task, reported_error)?;
        if let Some(error) = &task.error
            && error != &task.message
            && Some(error.as_str()) != reported_error
        {
            writeln!(output, "{}", TerminalEscaped(error))?;
        }
    }
    output.flush()?;
    Ok(())
}

fn write_scan_outcome(
    output: &mut impl Write,
    i18n: &Localizer,
    task: &ScanTaskStatus,
    reported_error: Option<&str>,
) -> Result<()> {
    if !task.message.is_empty() && Some(task.message.as_str()) != reported_error {
        writeln!(output, "{}", TerminalEscaped(&task.message))?;
    }
    if task.library_committed {
        writeln!(output, "{}", i18n.text("core-scan-library-committed"))?;
    } else if matches!(
        task.progress.phase,
        ScanPhase::Failed | ScanPhase::Cancelled
    ) {
        writeln!(output, "{}", i18n.text("core-scan-library-unconfirmed"))?;
    }
    if let Some(id) = task.playlist_id {
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-scan-playlist-created",
                &[("id", id.to_string().into())]
            )
        )?;
    }
    if (task.library_committed || task.playlist_id.is_some())
        && (task.error.is_some() || task.progress.phase == ScanPhase::Failed)
    {
        writeln!(output, "{}", i18n.text("core-scan-saved-warning"))?;
    }
    Ok(())
}

fn emit_relocation(
    i18n: &Localizer,
    instance_id: u32,
    task: &RelocationTaskStatus,
    json: bool,
    show_error: bool,
) -> Result<()> {
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(&task_json(instance_id, task.id, task)?)?
        )?;
    } else {
        let phase = i18n.text(match task.phase {
            RelocationPhase::Verifying => "core-verifying",
            RelocationPhase::Completed => "core-completed",
            RelocationPhase::Cancelled => "core-cancelled",
            RelocationPhase::Failed => "core-failed",
        });
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-relocation-status",
                &[
                    (
                        "id",
                        TaskReference {
                            instance_id,
                            id: task.id
                        }
                        .to_string()
                        .into()
                    ),
                    ("source", task.source_id.to_string().into()),
                    ("target", task.target_id.to_string().into()),
                    ("phase", phase.into()),
                ]
            )
        )?;
        if show_error && let Some(error) = &task.error {
            writeln!(output, "{}", TerminalEscaped(error))?;
        }
    }
    output.flush()?;
    Ok(())
}

fn wait_outcome(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    target: TaskTarget,
) -> Result<(u32, TaskOutcome)> {
    let (kind, id) = match target {
        TaskTarget::Scan { id } => ("scan", id),
        TaskTarget::Relocation { id } => ("relocation", id),
        TaskTarget::Processing { id } => ("processing", id),
    };
    let instance_id = instance_id(session)?;
    let reference = TaskReference { instance_id, id };
    let outcome = session.wait_task(target).with_context(|| {
        format!(
            "{}: rivu task status {kind} {reference}",
            i18n.text("core-task-wait-outcome-unavailable"),
        )
    })?;
    Ok((instance_id, outcome))
}

pub(super) fn wait_scan(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    let (instance, TaskOutcome::Scan(task)) = wait_outcome(i18n, session, TaskTarget::Scan { id })?
    else {
        bail!("IPC returned an unexpected task outcome");
    };
    let error = (task.progress.phase != ScanPhase::Completed).then(|| {
        task.error
            .as_deref()
            .or_else(|| (!task.message.is_empty()).then_some(task.message.as_str()))
            .unwrap_or(if task.progress.phase == ScanPhase::Cancelled {
                "Scan cancelled"
            } else {
                "Scan failed"
            })
    });
    emit_scan(i18n, instance, &task, json, error)?;
    if let Some(error) = error {
        bail!("{error}");
    }
    Ok(())
}

pub(super) fn wait_relocation(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    let (instance, TaskOutcome::Relocation(task)) =
        wait_outcome(i18n, session, TaskTarget::Relocation { id })?
    else {
        bail!("IPC returned an unexpected task outcome");
    };
    emit_relocation(
        i18n,
        instance,
        &task,
        json,
        task.phase == RelocationPhase::Completed,
    )?;
    match task.phase {
        RelocationPhase::Completed => Ok(()),
        _ => bail!(
            "{}",
            task.error
                .as_deref()
                .unwrap_or(if task.phase == RelocationPhase::Cancelled {
                    "Relocation cancelled"
                } else {
                    "Relocation failed"
                })
        ),
    }
}

pub(super) fn wait_processing(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    let (instance, TaskOutcome::Processing(task)) =
        wait_outcome(i18n, session, TaskTarget::Processing { id })?
    else {
        bail!("IPC returned an unexpected task outcome");
    };
    emit_processing(
        i18n,
        instance,
        &task,
        json,
        task.phase == ProcessingPhase::Complete,
    )?;
    match task.phase {
        ProcessingPhase::Complete => Ok(()),
        _ => bail!(
            "{}",
            task.error
                .as_deref()
                .unwrap_or(if task.phase == ProcessingPhase::Cancelled {
                    "Processing cancelled"
                } else {
                    "Processing failed"
                })
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Action, Args};
    use clap::Parser;

    #[test]
    fn task_commands_require_complete_bounded_identity() {
        for invalid in [
            "9",
            "0:9",
            "1:0",
            "4294967296:9",
            "1:18446744073709551616",
            "1:2:3",
            "+1:2",
            "1:-2",
        ] {
            assert!(invalid.parse::<TaskReference>().is_err(), "{invalid}");
        }
        for verb in ["status", "cancel"] {
            for kind in ["scan", "relocation", "processing"] {
                let args = Args::try_parse_from([
                    "rivu",
                    "task",
                    verb,
                    kind,
                    "4294967295:18446744073709551615",
                ])
                .unwrap();
                let Some(Action::Task(action)) = args.action else {
                    panic!("expected task command")
                };
                let reference = match action {
                    TaskAction::Status { id, .. } | TaskAction::Cancel { id, .. } => id,
                };
                assert_eq!(reference.instance_id, u32::MAX);
                assert_eq!(reference.id, u64::MAX);
                assert_eq!(reference.to_string(), "4294967295:18446744073709551615");
            }
        }
    }

    #[test]
    fn retained_processing_consumers_reject_bare_ids() {
        for arguments in [
            vec!["rivu", "processing", "audition", "9", "original"],
            vec!["rivu", "processing", "graph", "audition", "9", "1"],
            vec!["rivu", "processing", "graph", "export", "9", "1", "out.wav"],
        ] {
            assert!(Args::try_parse_from(arguments).is_err());
        }
    }
}

use super::{
    args::{TaskAction, TaskKind},
    output::{Terminal, show_ack, show_scan},
};
use anyhow::{Context, Result, bail, ensure};
use rivu::i18n::Localizer;
use rivu_protocol::{
    model::{
        Command, ProcessingPhase, ProcessingTaskStatus, Query, RelocationPhase,
        RelocationTaskStatus, ScanPhase, ScanTaskStatus,
    },
    response::ViewResponse,
};
use std::{path::Path, thread, time::Duration};

pub(super) fn run(i18n: &Localizer, data_dir: &Path, action: TaskAction, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    match action {
        TaskAction::Status { kind, id } => match kind {
            TaskKind::Scan => emit_scan(i18n, &scan_status(&mut session, id)?, json),
            TaskKind::Relocation => {
                emit_relocation(i18n, &relocation_status(&mut session, id)?, json)
            }
            TaskKind::Processing => {
                emit_processing(i18n, &processing_status(&mut session, id)?, json)
            }
        },
        TaskAction::Cancel { kind, id } => {
            let command = match kind {
                TaskKind::Scan => Command::CancelScan { id },
                TaskKind::Relocation => Command::CancelRelocation { id },
                TaskKind::Processing => Command::CancelProcessing { id },
            };
            let ack = session.request_ack(command)?;
            show_ack(i18n, &ack, json)
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
    task: &ProcessingTaskStatus,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        let phase = match task.phase {
            ProcessingPhase::Queued => "processing-queued",
            ProcessingPhase::Rendering => "processing-rendering",
            ProcessingPhase::Processing => "processing-processing",
            ProcessingPhase::Complete => "processing-complete",
            ProcessingPhase::Cancelled => "processing-cancelled",
            ProcessingPhase::Failed => "processing-failed",
        };
        println!(
            "{}",
            i18n.format(
                "core-processing-task",
                &[
                    ("id", task.id.to_string().into()),
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
        );
        if let Some(error) = &task.error {
            println!("{}", Terminal(error));
        }
    }
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

fn emit_scan(i18n: &Localizer, task: &ScanTaskStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        show_scan(i18n, &task.progress);
        if let Some(error) = &task.error {
            println!("{}", Terminal(error));
        }
    }
    Ok(())
}

fn emit_relocation(i18n: &Localizer, task: &RelocationTaskStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        let phase = i18n.text(match task.phase {
            RelocationPhase::Verifying => "core-verifying",
            RelocationPhase::Completed => "core-completed",
            RelocationPhase::Cancelled => "core-cancelled",
            RelocationPhase::Failed => "core-failed",
        });
        println!(
            "{}",
            i18n.format(
                "core-relocation-status",
                &[
                    ("id", task.id.to_string().into()),
                    ("source", task.source_id.to_string().into()),
                    ("target", task.target_id.to_string().into()),
                    ("phase", phase.into()),
                ]
            )
        );
        if let Some(error) = &task.error {
            println!("{}", Terminal(error));
        }
    }
    Ok(())
}

pub(super) fn wait_scan(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    loop {
        let task = scan_status(session, id)?;
        match task.progress.phase {
            ScanPhase::Completed => return emit_scan(i18n, &task, json),
            ScanPhase::Failed | ScanPhase::Cancelled => {
                emit_scan(i18n, &task, json)?;
                bail!("{}", task.error.as_deref().unwrap_or(&task.message));
            }
            _ => thread::sleep(Duration::from_millis(200)),
        }
    }
}

pub(super) fn wait_relocation(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    loop {
        let task = relocation_status(session, id)?;
        match task.phase {
            RelocationPhase::Verifying => thread::sleep(Duration::from_millis(200)),
            RelocationPhase::Completed => return emit_relocation(i18n, &task, json),
            RelocationPhase::Failed | RelocationPhase::Cancelled => {
                emit_relocation(i18n, &task, json)?;
                bail!(
                    "{}",
                    task.error.as_deref().unwrap_or("Relocation cancelled")
                );
            }
        }
    }
}

pub(super) fn wait_processing(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    id: u64,
    json: bool,
) -> Result<()> {
    loop {
        let task = processing_status(session, id)?;
        if task.phase.is_terminal() {
            emit_processing(i18n, &task, json)?;
            return match task.phase {
                ProcessingPhase::Complete => Ok(()),
                _ => bail!(
                    "{}",
                    task.error.as_deref().unwrap_or("Processing cancelled")
                ),
            };
        }
        thread::sleep(Duration::from_millis(150));
    }
}

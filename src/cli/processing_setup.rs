use super::args::{ProcessingSetup, SetupModel};
use anyhow::{Context, Result, bail, ensure};
use rivu_protocol::{
    model::{Command, NeuralPatch},
    response::StateSections,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
};

impl SetupModel {
    fn name(self) -> &'static str {
        match self {
            Self::Gtcrn => "gtcrn",
            Self::UlUnas => "ul-unas",
            Self::Moss => "moss",
            Self::Separate => "separate",
            Self::Df3 => "df3",
        }
    }

    fn native(self) -> bool {
        matches!(self, Self::Gtcrn | Self::UlUnas)
    }
}

#[derive(Deserialize)]
struct Installed {
    interpreter: PathBuf,
    installed_lock: PathBuf,
}

#[derive(Deserialize)]
struct Prepared {
    model_manifest: PathBuf,
    worker: Option<PathBuf>,
    interpreter: Option<PathBuf>,
}

#[derive(Serialize)]
struct SetupResult {
    model: &'static str,
    directory: PathBuf,
    environment_directory: Option<PathBuf>,
    installed_lock: Option<PathBuf>,
    model_manifest: PathBuf,
    worker: Option<PathBuf>,
    interpreter: Option<PathBuf>,
    activated: bool,
}

// Installed resources win. Development fallback is tied to this executable's
// checkout build tree, not the caller's working directory.
fn resources() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("Locating the Rivu executable")?;
    if let Some(prefix) = executable.parent().and_then(Path::parent) {
        let installed = prefix.join("share/rivu/audio_processing");
        if installed.join("prepare.py").is_file() {
            return Ok(installed);
        }
    }
    if let Some(directory) = executable.parent() {
        let source = directory.join(env!("RIVU_DEVELOPMENT_RESOURCES"));
        if source.join("prepare.py").is_file() {
            return Ok(source);
        }
    }
    bail!(
        "Audio setup resources are missing; install PREFIX/share/rivu/audio_processing beside the Rivu installation"
    )
}

fn script_result<T: serde::de::DeserializeOwned>(command: &mut Process, stage: &str) -> Result<T> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Starting {stage}"))?;
    if !output.status.success() {
        // Scripts reserve stdout for their result; preserve unexpected diagnostics
        // on failure without contaminating the CLI's machine-readable stdout.
        use std::io::Write;
        std::io::stderr().write_all(&output.stdout)?;
        bail!("{stage} failed ({})", output.status);
    }
    serde_json::from_slice(&output.stdout).with_context(|| format!("Reading {stage} JSON result"))
}

fn activate(data_dir: &Path, model: SetupModel, prepared: &Prepared) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
    let mut patch = NeuralPatch::default();
    if !model.native() {
        let worker = prepared
            .worker
            .as_ref()
            .context("Prepared Python bundle omitted worker")?;
        let state = session.get_state(StateSections {
            configuration: true,
            ..Default::default()
        })?;
        let configuration = state.configuration.context("Missing configuration state")?;
        let neural = &configuration.config.neural;
        let other = if matches!(model, SetupModel::Separate) {
            neural.speech_manifest.as_ref()
        } else {
            neural.separation_manifest.as_ref()
        };
        if neural.worker != *worker
            && let Some(other) = other
        {
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(other).with_context(|| {
                    format!("Reading other selected model {}", other.display())
                })?)
                .context("Reading other selected model manifest")?;
            ensure!(
                manifest.get("backend").and_then(|value| value.as_str()) == Some("onnx"),
                "Activation would replace the shared worker and invalidate the other selected Python model ({}); leave this bundle prepared or explicitly clear the other model first",
                other.display()
            );
        }
        patch.worker = Some(worker.clone());
        let interpreter = prepared
            .interpreter
            .clone()
            .context("Prepared Python bundle omitted interpreter")?;
        if matches!(model, SetupModel::Separate) {
            patch.separation_interpreter = Some(interpreter);
        } else {
            patch.speech_interpreter = Some(interpreter);
        }
    }
    if matches!(model, SetupModel::Separate) {
        patch.separation_manifest = Some(Some(prepared.model_manifest.clone()));
    } else {
        patch.speech_manifest = Some(Some(prepared.model_manifest.clone()));
    }
    let ack = session.request_ack(Command::NeuralEdit {
        patch: Box::new(patch),
    })?;
    ensure!(
        ack.is_success(),
        "Core rejected activation: {}",
        ack.error
            .as_deref()
            .unwrap_or("unsuccessful acknowledgement")
    );
    Ok(())
}

pub(super) fn run(data_dir: &Path, setup: ProcessingSetup, json: bool) -> Result<()> {
    let data_dir = super::client_path(data_dir.to_path_buf())?;
    let directory = super::client_path(
        setup
            .directory
            .unwrap_or_else(|| data_dir.join("neural").join(setup.model.name())),
    )?;
    // symlink_metadata also refuses dangling symlinks before any installation or download.
    match fs::symlink_metadata(&directory) {
        Ok(_) => bail!(
            "Bundle directory already exists (setup never overwrites): {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Inspecting bundle destination"),
    }
    let weights = setup.weights.map(super::client_path).transpose()?;
    if let Some(weights) = &weights {
        ensure!(
            weights.is_file(),
            "Weights file does not exist: {}",
            weights.display()
        );
    }
    let mut interpreter = setup.interpreter.map(super::client_path).transpose()?;
    if let Some(interpreter) = &interpreter {
        ensure!(
            interpreter.is_file(),
            "Interpreter does not exist: {}",
            interpreter.display()
        );
    }
    let scripts = resources()?;
    fs::create_dir_all(
        directory
            .parent()
            .context("Bundle directory needs a parent")?,
    )
    .context("Creating bundle parent directory")?;
    let mut environment_directory = None;
    let mut installed_lock = None;
    if !setup.model.native() && interpreter.is_none() {
        let mut environment = directory.as_os_str().to_os_string();
        environment.push(".environment");
        let environment = PathBuf::from(environment);
        let installed: Installed = script_result(
            Process::new("python3")
                .arg(scripts.join("install.py"))
                .arg(if matches!(setup.model, SetupModel::Df3) {
                    "df3"
                } else {
                    "default"
                })
                .arg("--directory")
                .arg(&environment)
                .arg("--temporary-directory")
                .arg(data_dir.join(".cache/neural")),
            "dependency installation",
        )?;
        interpreter = Some(installed.interpreter);
        installed_lock = Some(installed.installed_lock);
        environment_directory = Some(environment);
    }
    let mut command = Process::new("python3");
    command
        .arg(scripts.join("prepare.py"))
        .arg(if matches!(setup.model, SetupModel::UlUnas) {
            "ul_unas"
        } else {
            setup.model.name()
        })
        .arg("--directory")
        .arg(&directory);
    if let Some(weights) = weights {
        command.arg("--weights").arg(weights);
    } else {
        ensure!(
            setup.download,
            "Choose exactly one of --download or --weights"
        );
        command.arg("--download");
    }
    if !setup.model.native() {
        command
            .arg("--interpreter")
            .arg(interpreter.context("Missing Python interpreter")?);
    }
    let prepared: Prepared = script_result(&mut command, "model preparation")?;
    let activation = if setup.activate {
        activate(&data_dir, setup.model, &prepared)
    } else {
        Ok(())
    };
    let result = SetupResult {
        model: setup.model.name(),
        directory,
        environment_directory,
        installed_lock,
        model_manifest: prepared.model_manifest,
        worker: prepared.worker,
        interpreter: prepared.interpreter,
        activated: setup.activate && activation.is_ok(),
    };
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string(&result)?)?;
    } else {
        writeln!(output, "{}", serde_json::to_string_pretty(&result)?)?;
    }
    output.flush()?;
    activation.with_context(|| {
        format!(
            "Bundle prepared and retained at {}; manifest {}; activation failed",
            result.directory.display(),
            result.model_manifest.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_bundle_and_dangling_link_are_rejected_before_side_effects() {
        let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
        fs::create_dir_all(&cache).unwrap();
        let root = tempfile::tempdir_in(cache).unwrap();
        let bundle = root.path().join("bundle");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("manifest.json"), b"existing model").unwrap();
        let missing = root.path().join("missing");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        let data = root.path().join("data");
        for directory in [&bundle, &link] {
            let error = run(
                &data,
                ProcessingSetup {
                    model: SetupModel::Moss,
                    download: true,
                    weights: None,
                    directory: Some(directory.clone()),
                    interpreter: None,
                    activate: true,
                },
                true,
            )
            .unwrap_err();
            assert!(error.to_string().contains("already exists"));
            assert!(!data.exists());
            let mut environment = directory.as_os_str().to_os_string();
            environment.push(".environment");
            assert!(!Path::new(&environment).exists());
        }
        assert_eq!(
            fs::read(bundle.join("manifest.json")).unwrap(),
            b"existing model"
        );
        assert_eq!(fs::read_link(link).unwrap(), missing);
        assert!(!missing.exists());
    }
}

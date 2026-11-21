//! Startup orchestration only; supported database upgrades stay in rivu-migrate.
use crate::ipc::SocketOwner;
use anyhow::{Context, Result, ensure};
use rivu_format::database::{self, DatabaseState};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, process::Command, time::Instant};

fn inspect_database(path: &Path) -> Result<DatabaseState> {
    if !path
        .try_exists()
        .with_context(|| format!("Inspecting database {}", path.display()))?
    {
        return Ok(DatabaseState::Empty);
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Opening database read-only {}", path.display()))?;
    database::inspect(&connection)
}

pub(super) fn acquire_storage(data_dir: &Path, config_path: &Path) -> Result<SocketOwner> {
    let owner = SocketOwner::acquire(data_dir)?;
    let path = data_dir.join("library.db");
    let state = inspect_database(&path)?;
    if matches!(state, DatabaseState::Empty)
        || state
            == (DatabaseState::Versioned {
                schema_version: database::SCHEMA_VERSION,
            })
    {
        return Ok(owner);
    }

    let executable = std::env::current_exe()
        .context("Locating Rivu executable for automatic migration")?
        .with_file_name("rivu-migrate");
    let executable = if executable.try_exists()? {
        executable
    } else {
        "rivu-migrate".into()
    };
    tracing::info!(
        database = ?path,
        ?state,
        schema_version = database::SCHEMA_VERSION,
        ?executable,
        ?data_dir,
        config = ?config_path,
        "Invoking database migration tool"
    );
    // Keep queued Hello connections on the same listener throughout migration.
    let started = Instant::now();
    let result = async_io::block_on(async {
        let mut notice = MigrationNotice::default();
        let result = async {
            let mut child = Command::new(&executable)
                .arg("--data-dir").arg(data_dir)
                .arg("--config").arg(config_path)
                .arg("--inherit-listener").arg("apply")
                .stdin(owner.migration_stdio()?)
                .spawn()
                .with_context(|| format!("Starting {executable:?}; install a matching rivu-migrate beside rivu or on PATH"))?;
            let waiting = blocking::unblock(move || child.wait());
            let mut waiting = std::pin::pin!(waiting);
            if notice.enabled {
                use futures_lite::future::race;
                match race(async { Some(waiting.as_mut().await) }, async {
                    async_io::Timer::after(std::time::Duration::from_secs(1)).await;
                    None
                }).await {
                    Some(status) => status,
                    None => {
                        notice.update(Some("Upgrading media library…"), 30000).await;
                        waiting.await
                    }
                }
            } else {
                waiting.await
            }
            .context("Waiting for automatic migration")
        }.await.and_then(|status| verify_migration(status, &path, started));
        if notice.enabled {
            notice
                .update(
                    result.is_err().then_some("Media library upgrade failed"),
                    5000,
                )
                .await;
        }
        result
    });
    result?;
    Ok(owner)
}

fn verify_migration(status: std::process::ExitStatus, path: &Path, started: Instant) -> Result<()> {
    tracing::info!(
        %status,
        elapsed = ?started.elapsed(),
        "Migration tool exited"
    );
    ensure!(
        status.success(),
        "Automatic database migration failed ({status}); startup stopped; inspect the migration output and retained backups"
    );
    let state = inspect_database(path)?;
    ensure!(
        state
            == (DatabaseState::Versioned {
                schema_version: database::SCHEMA_VERSION
            }),
        "Migration tool succeeded but database {path:?} has {state:?}, expected format {}; startup stopped",
        database::SCHEMA_VERSION,
    );
    tracing::info!(
        schema_version = database::SCHEMA_VERSION,
        "Database format verified; resuming startup"
    );
    Ok(())
}

struct MigrationNotice {
    enabled: bool,
    id: u32,
    proxy: Option<zbus::Proxy<'static>>,
}

impl Default for MigrationNotice {
    fn default() -> Self {
        Self {
            enabled: std::env::var_os("RIVU_DESKTOP_BOOTSTRAP").is_some(),
            id: 0,
            proxy: None,
        }
    }
}

impl MigrationNotice {
    async fn update(&mut self, summary: Option<&str>, expiration: i32) {
        if let Some(proxy) = self.proxy.take() {
            let id = std::mem::take(&mut self.id);
            let _ = notification_deadline(async {
                proxy.call::<_, _, ()>("CloseNotification", &(id,)).await
            })
            .await;
        }
        let Some(summary) = summary else { return };
        if let Some((proxy, id)) = notification_deadline(async {
            let connection = zbus::Connection::session().await?;
            let bus = zbus::fdo::DBusProxy::new(&connection).await?;
            let owner = bus
                .get_name_owner(zbus::names::BusName::try_from(
                    "org.freedesktop.Notifications",
                )?)
                .await;
            let owner = match owner {
                Ok(owner) => owner,
                Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
                    bus.start_service_by_name(
                        zbus::names::WellKnownName::try_from("org.freedesktop.Notifications")?,
                        0,
                    )
                    .await?;
                    bus.get_name_owner(zbus::names::BusName::try_from(
                        "org.freedesktop.Notifications",
                    )?)
                    .await?
                }
                Err(error) => return Err(error.into()),
            };
            let proxy = zbus::Proxy::new_owned(
                connection,
                owner,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            let id = proxy
                .call(
                    "Notify",
                    &(
                        "Rivu",
                        0u32,
                        "",
                        summary,
                        "",
                        Vec::<&str>::new(),
                        std::collections::HashMap::<&str, zbus::Variant<'_>>::new(),
                        expiration,
                    ),
                )
                .await?;
            Ok((proxy, id))
        })
        .await
        {
            self.proxy = Some(proxy);
            self.id = id;
        }
    }
}

async fn notification_deadline<T>(call: impl Future<Output = zbus::Result<T>>) -> Option<T> {
    futures_lite::future::race(async { call.await.ok() }, async {
        async_io::Timer::after(std::time::Duration::from_millis(250)).await;
        None
    })
    .await
}

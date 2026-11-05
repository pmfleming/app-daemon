//! One process owns the store. Never repair, reset or import an unknown file.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use turso::Connection;

const APPLICATION_ID: i64 = 0x41504853; // APHS
const VERSION: i64 = 1;

pub(super) struct Database {
    pub(super) connection: Connection,
    _database: turso::Database,
    _file: Option<File>,
    // Released only after the database/connection have been dropped.
    _ownership: Option<Ownership>,
    path: Option<PathBuf>,
    pub(super) last_prune: Option<u64>,
    blocked: bool,
    #[cfg(test)]
    pub(super) fault: Option<Fault>,
    #[cfg(test)]
    _temporary: Option<tempfile::TempDir>,
}

struct Ownership(File);

impl Drop for Ownership {
    fn drop(&mut self) {
        // Closing alone waits for every duplicated/inherited descriptor. A
        // concurrent fork may retain one until exec even with CLOEXEC set.
        // This field drops after the connection/database, so explicitly release
        // the finished owner's lock before its descriptor is closed.
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::Database;
    use futures::executor::block_on;

    #[test]
    fn dropping_owner_releases_lock_even_with_an_inherited_descriptor() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("history.db");
        let owner = block_on(Database::open(&path))?;
        // A descriptor duplicated by fork can outlive the parent until exec,
        // including a CLOEXEC descriptor in a concurrent process-spawn window.
        let inherited = owner._ownership.as_ref().unwrap().0.try_clone()?;
        assert!(block_on(Database::open(&path)).is_err());
        drop(owner);
        let reopened = block_on(Database::open(&path))?;
        assert!(block_on(Database::open(&path)).is_err());
        drop(inherited);
        assert!(block_on(Database::open(&path)).is_err());
        drop(reopened);
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Fault {
    Write,
    Commit,
    Rollback,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn private_file(path: &Path, create: bool, exclusive: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .create_new(exclusive)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == rustix::process::geteuid().as_raw() && meta.nlink() == 1,
        "unsafe history file {}",
        path.display()
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

impl Database {
    pub(super) async fn open(path: &Path) -> Result<Self> {
        Self::open_inner(path).await.with_context(|| format!("open application history {}; incompatible/corrupt stores are not reset or migrated", path.display()))
    }

    async fn open_inner(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let meta = fs::symlink_metadata(parent)?;
        ensure!(
            meta.is_dir() && meta.uid() == rustix::process::geteuid().as_raw(),
            "history directory must be owned by this user and not a symlink"
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let ownership = private_file(&sidecar(path, ".lock"), true, false)?;
        ownership
            .try_lock()
            .context("history database already owned by another process")?;
        let ownership = Ownership(ownership);
        let (file, fresh) = match private_file(path, false, true) {
            Ok(file) => (file, true),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                (private_file(path, false, false)?, false)
            }
            Err(error) => return Err(error),
        };
        ensure!(
            fresh || file.metadata()?.len() > 0 || sidecar(path, "-wal").try_exists()?,
            "preexisting empty history database; choose a fresh path"
        );
        for suffix in ["-wal", "-shm"] {
            match fs::symlink_metadata(sidecar(path, suffix)) {
                Ok(_) => {
                    ensure!(!fresh, "orphaned history database sidecar");
                    drop(private_file(&sidecar(path, suffix), false, false)?);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let database =
            turso::Builder::new_local(path.to_str().context("non-UTF8 history database path")?)
                .with_io(turso::IoBackend::Syscall)
                .experimental_multiprocess_wal(true)
                .build()
                .await?;
        let held = file.metadata()?;
        let opened = fs::symlink_metadata(path)?;
        ensure!(
            opened.is_file() && opened.ino() == held.ino() && opened.dev() == held.dev(),
            "history database changed while opening"
        );
        let mut store = Self {
            connection: database.connect()?,
            _database: database,
            _file: Some(file),
            _ownership: Some(ownership),
            path: Some(path.to_owned()),
            last_prune: None,
            blocked: false,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            _temporary: None,
        };
        store.initialize(fresh).await?;
        store.harden_sidecars()?;
        tracing::info!(path = %path.display(), engine = "turso", schema_version = VERSION, created = fresh, "application history opened");
        Ok(store)
    }

    #[cfg(test)]
    pub(super) async fn temporary() -> Result<Self> {
        // Use unique on-disk paths with syscall I/O, never a literal :memory:
        // filename. This also exercises the production WAL/permission policy.
        let directory = tempfile::tempdir()?;
        let mut store = Self::open(&directory.path().join("history.db")).await?;
        store._temporary = Some(directory);
        Ok(store)
    }

    async fn initialize(&mut self, fresh: bool) -> Result<()> {
        self.connection.busy_timeout(Duration::from_secs(5))?;
        self.connection
            .execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-8192;")
            .await?;
        if fresh {
            self.begin().await?;
            let result = async {
                let metric_columns = super::summary::sql_columns();
                self.connection.execute_batch(format!("
                    CREATE TABLE resource_points (
                        target_id TEXT NOT NULL, timestamp_ms INTEGER NOT NULL CHECK(timestamp_ms >= 0),
                        start_ms INTEGER NOT NULL CHECK(start_ms >= 0 AND start_ms <= timestamp_ms),
                        payload TEXT NOT NULL, energy_confidence INTEGER,
                        {metric_columns}, PRIMARY KEY(target_id, timestamp_ms)
                    ) STRICT;
                    CREATE INDEX resource_retention ON resource_points(timestamp_ms);
                    CREATE TABLE energy_points (
                        target_id TEXT NOT NULL, timestamp_ms INTEGER NOT NULL CHECK(timestamp_ms >= 0),
                        energy_mwh REAL NOT NULL CHECK(energy_mwh >= 0),
                        energy_source TEXT NOT NULL, energy_confidence TEXT NOT NULL,
                        PRIMARY KEY(target_id, timestamp_ms)
                    ) STRICT;
                    CREATE INDEX energy_retention ON energy_points(timestamp_ms);
                    CREATE TABLE cursor_epochs (target_id TEXT PRIMARY KEY, epoch TEXT NOT NULL) STRICT;
                    PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={VERSION};
                ")).await?;
                Ok(())
            }.await;
            self.finish(result).await?;
        } else {
            ensure!(
                self.integer("PRAGMA application_id").await? == APPLICATION_ID
                    && self.integer("PRAGMA user_version").await? == VERSION,
                "unsupported history database identity/schema"
            );
            // Validate expected columns without reading or decoding the retained history.
            self.connection.query(format!("SELECT target_id,timestamp_ms,start_ms,payload,energy_confidence,{} FROM resource_points LIMIT 0", super::summary::sql_column_names().join(",")), ()).await?;
            self.connection.query("SELECT target_id,timestamp_ms,energy_mwh,energy_source,energy_confidence FROM energy_points LIMIT 0", ()).await?;
            self.connection
                .query("SELECT target_id,epoch FROM cursor_epochs LIMIT 0", ())
                .await?;
        }
        Ok(())
    }

    pub(super) fn healthy(&self) -> Result<()> {
        ensure!(
            !self.blocked,
            "history database fenced after an uncertain write; restart required"
        );
        Ok(())
    }

    pub(super) async fn integer(&self, sql: &str) -> Result<i64> {
        Ok(self
            .connection
            .query(sql, ())
            .await?
            .next()
            .await?
            .context("missing database scalar")?
            .get(0)?)
    }

    pub(super) async fn begin(&self) -> Result<()> {
        self.healthy()?;
        self.connection.execute("BEGIN IMMEDIATE", ()).await?;
        Ok(())
    }

    pub(super) async fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        let result = match result {
            Ok(value) => {
                let commit = self.connection.execute("COMMIT", ()).await;
                #[cfg(test)]
                let commit = if self.fault == Some(Fault::Commit) {
                    self.fault = None;
                    Err(turso::Error::Error("injected ambiguous commit".into()))
                } else {
                    commit
                };
                match commit {
                    Ok(_) => Ok(value),
                    Err(error) => {
                        self.blocked = true;
                        Err(error.into())
                    }
                }
            }
            Err(error) => Err(error),
        };
        if result.is_err() && !self.connection.is_autocommit().unwrap_or(false) {
            let rollback = self.connection.execute("ROLLBACK", ()).await;
            #[cfg(test)]
            let rollback = if self.fault == Some(Fault::Rollback) {
                self.fault = None;
                Err(turso::Error::Error("injected rollback failure".into()))
            } else {
                rollback
            };
            if rollback.is_err() {
                self.blocked = true;
            }
        }
        if let Err(error) = self.harden_sidecars() {
            self.blocked = true;
            return Err(error).context("history sidecar protection failed");
        }
        result
    }

    fn harden_sidecars(&self) -> Result<()> {
        if let Some(path) = &self.path {
            for suffix in ["-wal", "-shm"] {
                let path = sidecar(path, suffix);
                match fs::symlink_metadata(&path) {
                    Ok(_) => {
                        drop(private_file(&path, false, false)?);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    pub(super) async fn checkpoint(&self) -> Result<()> {
        self.healthy()?;
        let row = self
            .connection
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await?
            .next()
            .await?
            .context("missing checkpoint result")?;
        ensure!(
            row.get::<i64>(0)? == 0 && row.get::<i64>(1)? == row.get::<i64>(2)?,
            "history checkpoint incomplete"
        );
        self.harden_sidecars()
    }
}

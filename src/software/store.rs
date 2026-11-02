//! A pristine Turso store, owned exclusively by app-daemon. No JSON import,
//! automatic schema upgrade, reset, or second writer. All writes are atomic.
use super::Snapshot;
use crate::settings::{ApplicationSettings, SettingsStore};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const APPLICATION_ID: i64 = 0x41504341; // APCA
const VERSION: i64 = 1;
pub(crate) struct Store {
    connection: turso::Connection,
    _database: turso::Database,
    _file: File,
    _ownership: File,
    path: PathBuf,
    blocked: bool,
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
        "unsafe catalog file"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
impl Store {
    #[cfg(test)]
    pub(crate) fn test_sql(&self, sql: &str) {
        futures::executor::block_on(self.connection.execute_batch(sql)).unwrap();
    }
    pub(crate) fn open(path: &Path) -> Result<Self> {
        futures::executor::block_on(Self::open_async(path))
    }
    async fn open_async(path: &Path) -> Result<Self> {
        let parent = path.parent().context("catalog path needs a parent")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let meta = fs::symlink_metadata(parent)?;
        ensure!(
            meta.is_dir() && meta.uid() == rustix::process::geteuid().as_raw(),
            "catalog directory must be owned by this user"
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let ownership = private_file(&sidecar(path, ".lock"), true, false)?;
        ownership
            .try_lock()
            .context("catalog already owned by another process")?;
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
            "preexisting empty catalog; use a pristine database path"
        );
        for suffix in ["-wal", "-shm"] {
            if fs::symlink_metadata(sidecar(path, suffix)).is_ok() {
                ensure!(!fresh, "orphaned catalog sidecar");
                private_file(&sidecar(path, suffix), false, false)?;
            }
        }
        let database = turso::Builder::new_local(path.to_str().context("non-UTF8 catalog path")?)
            .with_io(turso::IoBackend::Syscall)
            .build()
            .await?;
        let held = file.metadata()?;
        let opened = fs::symlink_metadata(path)?;
        ensure!(
            opened.is_file() && opened.ino() == held.ino() && opened.dev() == held.dev(),
            "catalog changed while opening"
        );
        let mut store = Self {
            connection: database.connect()?,
            _database: database,
            _file: file,
            _ownership: ownership,
            path: path.into(),
            blocked: false,
        };
        store
            .connection
            .busy_timeout(std::time::Duration::from_secs(5))?;
        store
            .connection
            .execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-4096;")
            .await?;
        if fresh {
            store.connection.execute("BEGIN IMMEDIATE", ()).await?;
            let result = async {
                store.connection.execute_batch(format!("
                    CREATE TABLE applications (id TEXT PRIMARY KEY, kind TEXT NOT NULL, category TEXT NOT NULL, sort_name TEXT NOT NULL, payload TEXT NOT NULL) STRICT;
                    CREATE INDEX application_groups ON applications(kind,category,sort_name,id);
                    CREATE TABLE preferences (id TEXT PRIMARY KEY, payload TEXT NOT NULL) STRICT;
                    CREATE TABLE snapshot (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL) STRICT;
                    CREATE TABLE worker_observation (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL) STRICT;
                    PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={VERSION};
                ")).await?;
                Ok(())
            }.await;
            store.finish(result).await?;
        } else {
            ensure!(
                store.integer("PRAGMA application_id").await? == APPLICATION_ID
                    && store.integer("PRAGMA user_version").await? == VERSION,
                "unsupported catalog identity/schema; use a pristine database path"
            );
            store
                .connection
                .query(
                    "SELECT id,kind,category,sort_name,payload FROM applications LIMIT 0",
                    (),
                )
                .await?;
            store
                .connection
                .query("SELECT id,payload FROM preferences LIMIT 0", ())
                .await?;
            store
                .connection
                .query("SELECT id,payload FROM snapshot LIMIT 0", ())
                .await?;
            store
                .connection
                .query("SELECT id,payload FROM worker_observation LIMIT 0", ())
                .await?;
        }
        store.harden()?;
        Ok(store)
    }
    async fn integer(&self, sql: &str) -> Result<i64> {
        Ok(self
            .connection
            .query(sql, ())
            .await?
            .next()
            .await?
            .context("missing scalar")?
            .get(0)?)
    }
    fn healthy(&self) -> Result<()> {
        ensure!(
            !self.blocked,
            "catalog write outcome uncertain; restart required"
        );
        Ok(())
    }
    fn harden(&self) -> Result<()> {
        for suffix in ["-wal", "-shm"] {
            let path = sidecar(&self.path, suffix);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    private_file(&path, false, false)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    async fn finish(&mut self, result: Result<()>) -> Result<()> {
        let result = match result {
            Ok(()) => self
                .connection
                .execute("COMMIT", ())
                .await
                .map(|_| ())
                .map_err(|e| {
                    self.blocked = true;
                    e.into()
                }),
            Err(e) => Err(e),
        };
        if result.is_err()
            && !self.connection.is_autocommit().unwrap_or(false)
            && self.connection.execute("ROLLBACK", ()).await.is_err()
        {
            self.blocked = true;
        }
        if let Err(error) = self.harden() {
            self.blocked = true;
            return Err(error);
        }
        result
    }
    pub(crate) fn settings(&self) -> Result<SettingsStore> {
        self.healthy()?;
        futures::executor::block_on(async {
            let mut rows = self
                .connection
                .query("SELECT id,payload FROM preferences", ())
                .await?;
            let mut entries = BTreeMap::new();
            while let Some(row) = rows.next().await? {
                let settings: ApplicationSettings = serde_json::from_str(&row.get::<String>(1)?)?;
                ensure!(
                    settings.category.is_empty()
                        || crate::settings::workspace_for_category(&settings.category).is_some(),
                    "invalid catalog category"
                );
                entries.insert(row.get::<String>(0)?, settings);
            }
            Ok(SettingsStore::from_entries(entries))
        })
    }
    pub(crate) fn snapshot(&self) -> Result<Option<Snapshot>> {
        self.healthy()?;
        futures::executor::block_on(async {
            let Some(row) = self
                .connection
                .query("SELECT payload FROM snapshot WHERE id=1", ())
                .await?
                .next()
                .await?
            else {
                return Ok(None);
            };
            let mut snapshot: Snapshot = serde_json::from_str(&row.get::<String>(0)?)?;
            let mut rows = self.connection.query("SELECT payload FROM applications ORDER BY CASE kind WHEN 'desktop-application' THEN 0 WHEN 'desktop-shortcut' THEN 1 WHEN 'command-line' THEN 2 WHEN 'system' THEN 3 ELSE 4 END, category,sort_name,id", ()).await?;
            while let Some(row) = rows.next().await? {
                snapshot
                    .entries
                    .push(serde_json::from_str(&row.get::<String>(0)?)?);
            }
            Ok(Some(snapshot))
        })
    }
    pub(crate) fn observations(&self) -> Result<super::Observations> {
        self.healthy()?;
        futures::executor::block_on(async {
            match self
                .connection
                .query("SELECT payload FROM worker_observation WHERE id=1", ())
                .await?
                .next()
                .await?
            {
                Some(row) => Ok(serde_json::from_str(&row.get::<String>(0)?)?),
                None => Ok(Default::default()),
            }
        })
    }
    // Runs while holding an owned mutex in spawn_blocking: cancellation cannot
    // interrupt a transaction or publish an uncommitted category change.
    pub(crate) fn replace(
        &mut self,
        snapshot: &Snapshot,
        settings: Option<&SettingsStore>,
        observations: &super::Observations,
    ) -> Result<()> {
        self.healthy()?;
        ensure!(
            snapshot.entries.len() <= 8192,
            "software catalog exceeds 8192 records"
        );
        futures::executor::block_on(async {
            self.connection.execute("BEGIN IMMEDIATE", ()).await?;
            let result = async {
                self.connection.execute("DELETE FROM applications", ()).await?;
                for record in &snapshot.entries {
                    self.connection.execute("INSERT INTO applications(id,kind,category,sort_name,payload) VALUES (?1,?2,?3,?4,?5)",
                        turso::params![record.id.clone(), record.kind.clone(), record.category.clone(), record.name.to_lowercase(), serde_json::to_string(record)?]).await?;
                }
                let mut metadata = snapshot.clone(); metadata.entries.clear();
                self.connection.execute("INSERT INTO snapshot(id,payload) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [serde_json::to_string(&metadata)?]).await?;
                self.connection.execute("INSERT INTO worker_observation(id,payload) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [serde_json::to_string(observations)?]).await?;
                if let Some(settings) = settings {
                    self.connection.execute("DELETE FROM preferences", ()).await?;
                    for (id, preference) in settings.entries() {
                        self.connection.execute("INSERT INTO preferences(id,payload) VALUES (?1,?2)",
                            turso::params![id.clone(), serde_json::to_string(preference)?]).await?;
                    }
                }
                Ok(())
            }.await;
            self.finish(result).await
        })
    }
}

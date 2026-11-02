//! Rust-only routing and durable launch receipts. Recovery never starts a process.
use crate::{model::OperationResult, settings::ApplicationSettings};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shelllist_daemon_core::{
    AtomicFilePolicy, read_bytes_bounded,
    workspace_wire::{self as wire, Context, Create, LaunchTarget, Lease},
};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub(super) async fn call(method: &str, params: impl Serialize) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.laufan.BarDaemon",
            "/org/laufan/BarDaemon",
            "org.laufan.BarDaemon2",
        )
        .await?;
        let reply: String = proxy
            .call("Call", &(method, serde_json::to_string(&params)?))
            .await?;
        let value: Value = serde_json::from_str(&reply)?;
        ensure!(
            value["ok"] == true,
            "workspace owner rejected request: {}",
            value["error"]
        );
        value["data"]
            .get("workspace_operation")
            .cloned()
            .context("workspace owner lacks lifecycle protocol")
    })
    .await
    .context("workspace owner outcome unknown; do not replay effects")?
}

pub(super) async fn capture() -> Result<Context> {
    let response = call("workspace.context", json!({})).await?;
    ensure!(
        response["version"] == wire::VERSION,
        "unsupported workspace protocol"
    );
    Ok(serde_json::from_value(response["context"].clone())?)
}

pub(super) fn destination(
    target: LaunchTarget,
    settings: Option<&ApplicationSettings>,
    context: &Context,
    desktop_action: bool,
) -> Option<i64> {
    match target {
        LaunchTarget::Current => Some(context.workspace_id),
        LaunchTarget::New => None,
        LaunchTarget::Default => {
            if !desktop_action && settings.is_some_and(|s| s.new_workspace) {
                return None;
            }
            Some(
                settings
                    .and_then(|s| s.workspace_id.as_ref())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(context.workspace_id),
            )
        }
    }
}

pub(super) struct Reservation(pub Lease);
impl Drop for Reservation {
    fn drop(&mut self) {
        let lease = self.0.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = call("workspace.lease.release", lease).await;
            });
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    owner: Option<String>,
    request: Value,
    result: OperationResult,
    reservation: Option<Create>,
    receipt: Option<Value>,
    #[serde(default)]
    baseline: Option<Value>,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    session: String,
    records: BTreeMap<String, Record>,
}

impl Journal {
    fn read(path: &std::path::Path, session: &str) -> Result<Self> {
        let mut journal: Self = match read_bytes_bounded(path, 16 * 1024 * 1024)? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => Self {
                version: 1,
                session: session.into(),
                records: BTreeMap::new(),
            },
        };
        ensure!(
            journal.version == 1 && journal.session == session && journal.records.len() <= 4096,
            "invalid launch journal"
        );
        for record in journal.records.values_mut() {
            if matches!(record.result.status.as_str(), "accepted" | "running") {
                record.result.status = "unknown".into();
                record.result.message = "Daemon restarted; launch outcome may be indeterminate. Inspect windows; do not automatically relaunch.".into();
                record.result.revision += 1;
            }
        }
        Ok(journal)
    }
}

#[derive(Default)]
pub(super) struct LaunchJournal {
    loaded: Option<(PathBuf, Journal)>,
    poisoned: bool,
}

impl LaunchJournal {
    fn load(&mut self) -> Result<&mut Journal> {
        ensure!(!self.poisoned, "launch journal unavailable until restart");
        if self.loaded.is_none() {
            let client = shelllist_hyprland::Client::default();
            let session = client
                .instance_signature()?
                .context("compositor session required")?
                .to_owned();
            let root = std::env::var_os("XDG_RUNTIME_DIR").context("runtime directory required")?;
            let path = PathBuf::from(root)
                .join("app-daemon")
                .join(format!("launches-{session}.json"));
            let journal = Journal::read(&path, &session)?;
            self.loaded = Some((path, journal));
        }
        Ok(&mut self.loaded.as_mut().unwrap().1)
    }

    async fn save(&mut self) -> Result<()> {
        ensure!(!self.poisoned, "launch journal unavailable");
        let (path, journal) = self.loaded.as_ref().context("launch journal not loaded")?;
        let bytes = serde_json::to_vec(journal)?;
        ensure!(bytes.len() <= 16 * 1024 * 1024, "launch journal full");
        if let Err(error) = shelllist_daemon_tokio::write_bytes_atomic_async(
            path.clone(),
            bytes,
            AtomicFilePolicy::PRIVATE,
        )
        .await
        {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    pub fn lookup(
        &mut self,
        id: &str,
        owner: Option<&str>,
        request: Option<&Value>,
    ) -> Result<Option<OperationResult>> {
        let Some(record) = self.load()?.records.get(id) else {
            return Ok(None);
        };
        ensure!(
            record.owner.as_deref() == owner,
            "operation belongs to another caller"
        );
        if let Some(request) = request {
            ensure!(
                &record.request == request,
                "request key reused with different parameters"
            );
        }
        Ok(Some(record.result.clone()))
    }

    pub async fn admit(
        &mut self,
        owner: Option<String>,
        request: Value,
        result: OperationResult,
    ) -> Result<()> {
        let journal = self.load()?;
        ensure!(
            journal.records.len() < 4096,
            "launch journal admission limit reached"
        );
        ensure!(
            !journal.records.contains_key(&result.id),
            "duplicate launch operation"
        );
        journal.records.insert(
            result.id.clone(),
            Record {
                owner,
                request,
                result,
                reservation: None,
                receipt: None,
                baseline: None,
            },
        );
        self.save().await
    }

    pub async fn reserve(&mut self, id: &str, request: Create) -> Result<()> {
        self.load()?
            .records
            .get_mut(id)
            .context("launch not admitted")?
            .reservation = Some(request);
        self.save().await
    }

    pub async fn receipt(
        &mut self,
        id: &str,
        receipt: &crate::launch::LaunchReceipt,
    ) -> Result<()> {
        let value = json!({"launch":receipt, "unit":receipt.unit, "roots":receipt.ownership_roots().collect::<Vec<_>>()});
        self.load()?
            .records
            .get_mut(id)
            .context("launch not admitted")?
            .receipt = Some(value);
        self.save().await
    }

    pub async fn handoff(&mut self, id: &str, baseline: Value) -> Result<()> {
        self.load()?
            .records
            .get_mut(id)
            .context("launch not admitted")?
            .baseline = Some(baseline);
        self.save().await
    }

    pub async fn update(&mut self, result: &OperationResult) -> Result<()> {
        // Non-launch operations retain their existing in-memory registry.
        let Some((_, journal)) = self.loaded.as_mut() else {
            return Ok(());
        };
        let Some(record) = journal.records.get_mut(&result.id) else {
            return Ok(());
        };
        record.result = result.clone();
        self.save().await
    }

    pub async fn recover(&mut self) -> Result<()> {
        let requests: Vec<_> = self
            .load()?
            .records
            .values()
            .filter(|r| r.result.status == "unknown")
            .filter_map(|r| r.reservation.clone())
            .collect();
        self.save().await?;
        for request in requests {
            if let Ok(value) = call(
                "workspace.operation.get",
                wire::Lookup {
                    session: request.context.session,
                    request_id: request.request_id,
                },
            )
            .await
                && let Some(lease) = value.get("lease")
            {
                let _ = call("workspace.lease.release", lease).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(path: PathBuf) -> LaunchJournal {
        let loaded = Journal::read(&path, "test-session").unwrap();
        LaunchJournal {
            loaded: Some((path, loaded)),
            poisoned: false,
        }
    }

    fn accepted() -> OperationResult {
        super::super::action::operation_result(
            "operation-key".into(),
            &super::super::ExecuteParams {
                target_id: "editor".into(),
                action: super::super::ApplicationAction::Launch,
                window_id: None,
                desktop_action_id: None,
                expected_revision: None,
                workspace_id: None,
                launch_target: LaunchTarget::New,
                request_id: Some("key".into()),
            },
            "accepted",
            "accepted".into(),
            None,
        )
    }

    #[tokio::test]
    async fn durable_admission_recovers_unknown_without_replaying_or_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launches.json");
        let request = json!({"launch_target":"new"});
        let mut owner = journal(path.clone());
        owner
            .admit(Some("caller".into()), request.clone(), accepted())
            .await
            .unwrap();
        assert!(
            owner
                .admit(Some("caller".into()), request.clone(), accepted())
                .await
                .is_err()
        );
        let original = std::fs::read(&path).unwrap();
        let mut restarted = journal(path.clone());
        assert_eq!(
            restarted
                .lookup("operation-key", Some("caller"), Some(&request))
                .unwrap()
                .unwrap()
                .status,
            "unknown"
        );
        assert!(
            restarted
                .lookup("operation-key", Some("other"), None)
                .is_err()
        );
        assert!(
            restarted
                .lookup("operation-key", Some("caller"), Some(&json!({})))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(Journal::read(&path, "other-session").is_err());
        std::fs::write(&path, "broken").unwrap();
        assert!(Journal::read(&path, "test-session").is_err());
    }

    #[tokio::test]
    async fn failed_persistence_fences_further_admission() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launches.json");
        let mut owner = journal(path.clone());
        std::fs::create_dir(&path).unwrap();
        assert!(owner.admit(None, json!({}), accepted()).await.is_err());
        assert!(owner.poisoned);
        std::fs::remove_dir(&path).unwrap();
        assert!(owner.admit(None, json!({}), accepted()).await.is_err());
    }
    #[test]
    fn routing_precedence_and_desktop_action_exception() {
        let context = Context {
            session: "s".into(),
            monitor: "m".into(),
            workspace_id: 17,
        };
        let settings = ApplicationSettings {
            category: "code".into(),
            workspace_id: Some("3".into()),
            new_workspace: true,
            category_set: true,
        };
        assert_eq!(
            destination(LaunchTarget::Default, Some(&settings), &context, false),
            None
        );
        assert_eq!(
            destination(LaunchTarget::Default, Some(&settings), &context, true),
            Some(3)
        );
        assert_eq!(
            destination(LaunchTarget::Current, Some(&settings), &context, false),
            Some(17)
        );
        assert_eq!(destination(LaunchTarget::New, None, &context, false), None);
        assert_eq!(
            destination(LaunchTarget::Default, None, &context, false),
            Some(17)
        );
    }
}

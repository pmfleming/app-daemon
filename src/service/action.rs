use std::time::{Duration, Instant};

use anyhow::Context;

use serde::Deserialize;

use tokio::time;

use crate::{
    catalog::Catalog,
    hyprland::{self, Client, Snapshot},
    launch::{self, LaunchReceipt},
    model::OperationResult,
};

use super::{
    ExecuteParams,
    query::{resolve_target, target_window},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplicationAction {
    Activate,
    Launch,
    FocusWindow,
    Close,
    CloseWindow,
    MoveToWorkspace,
    DesktopAction,
}

impl ApplicationAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Launch => "launch",
            Self::FocusWindow => "focus-window",
            Self::Close => "close",
            Self::CloseWindow => "close-window",
            Self::MoveToWorkspace => "move-to-workspace",
            Self::DesktopAction => "desktop-action",
        }
    }
}

pub(super) struct ActionOutcome {
    pub(super) message: String,
    pub(super) launch: Option<LaunchReceipt>,
}

pub(super) fn operation_result(
    id: String,
    params: &ExecuteParams,
    status: &str,
    message: String,
    launch: Option<LaunchReceipt>,
) -> OperationResult {
    OperationResult {
        id,
        action: params.action.as_str().into(),
        target_id: params.target_id.clone(),
        status: status.into(),
        message,
        launch_backend: launch.as_ref().map(|receipt| receipt.backend.clone()),
        launch_scope: launch.map(|receipt| receipt.scope),
    }
}

impl ActionOutcome {
    fn new(catalog: &Catalog, target_id: &str, verb: &str, launch: Option<LaunchReceipt>) -> Self {
        Self {
            message: format!("{verb} {}", display_name(catalog, target_id)),
            launch,
        }
    }
}

pub(super) async fn execute_action(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<ActionOutcome> {
    params.action.execute(catalog, windows, params).await
}

impl ApplicationAction {
    async fn execute(
        self,
        catalog: &Catalog,
        windows: &Snapshot,
        params: &ExecuteParams,
    ) -> anyhow::Result<ActionOutcome> {
        let target_id = &params.target_id;
        match self {
            Self::Activate => activate(catalog, windows, target_id).await,
            Self::Launch => launch(catalog, target_id)
                .await
                .map(|launch| ActionOutcome::new(catalog, target_id, "Launched", Some(launch))),
            Self::FocusWindow => focus_window(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Focused", None)),
            Self::Close => close_application(catalog, windows, target_id)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Closed", None)),
            Self::CloseWindow => close_window(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Closed", None)),
            Self::MoveToWorkspace => move_to_workspace(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Moved", None)),
            Self::DesktopAction => desktop_action(catalog, params)
                .await
                .map(|launch| ActionOutcome::new(catalog, target_id, "Started", Some(launch))),
        }
    }
}

async fn desktop_action(
    catalog: &Catalog,
    params: &ExecuteParams,
) -> anyhow::Result<LaunchReceipt> {
    let action = params
        .desktop_action_id
        .as_deref()
        .context("desktop_action_id is required")?;
    launch_action(catalog, &params.target_id, action).await
}

async fn activate(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> anyhow::Result<ActionOutcome> {
    if let Some(window) = target_window(catalog, windows, target_id) {
        hyprland::focus(&window.address).await?;
        return Ok(ActionOutcome::new(catalog, target_id, "Focused", None));
    }
    let launch = launch(catalog, target_id).await?;
    let verb = if focus_launched_window(catalog, target_id).await? {
        "Launched and focused"
    } else {
        "Launched"
    };
    Ok(ActionOutcome::new(catalog, target_id, verb, Some(launch)))
}

async fn focus_launched_window(catalog: &Catalog, target_id: &str) -> anyhow::Result<bool> {
    const FOCUS_TIMEOUT: Duration = Duration::from_secs(8);
    const FOCUS_RETRY_INTERVAL: Duration = Duration::from_millis(100);

    let deadline = Instant::now() + FOCUS_TIMEOUT;
    loop {
        let windows = Snapshot::load().await;
        if let Some(window) = target_window(catalog, &windows, target_id) {
            hyprland::focus(&window.address).await?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        time::sleep(FOCUS_RETRY_INTERVAL).await;
    }
}

async fn focus_window(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    hyprland::focus(address).await
}

async fn close_window(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    hyprland::close(address).await
}

async fn move_to_workspace(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    let workspace = params
        .workspace_id
        .as_deref()
        .context("workspace_id is required")?;
    hyprland::move_to_workspace(address, workspace).await
}

fn target_address<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<&'a str> {
    Ok(&target_instance(catalog, windows, params)?.address)
}

async fn close_application(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> anyhow::Result<()> {
    let addresses = application_window_addresses(catalog, windows, target_id);
    anyhow::ensure!(!addresses.is_empty(), "application is no longer running");
    for address in addresses {
        hyprland::close(&address).await?;
    }
    Ok(())
}

pub(super) fn application_window_addresses(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> Vec<String> {
    windows
        .clients
        .iter()
        .filter(|window| resolve_target(catalog, window) == target_id)
        .map(|window| window.address.clone())
        .collect()
}

fn target_instance<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<&'a Client> {
    let id = params
        .window_id
        .as_deref()
        .context("window_id is required")?;
    let window = windows
        .by_window_id(id)
        .context("window is no longer available")?;
    anyhow::ensure!(
        resolve_target(catalog, window) == params.target_id,
        "window no longer belongs to the selected application"
    );
    Ok(window)
}

fn display_name(catalog: &Catalog, target_id: &str) -> String {
    catalog.by_id(target_id).map_or_else(
        || {
            target_id
                .strip_prefix("window-group:")
                .unwrap_or(target_id)
                .to_owned()
        },
        |entry| entry.name.clone(),
    )
}

async fn launch(catalog: &Catalog, target_id: &str) -> anyhow::Result<LaunchReceipt> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    if entry.requires_terminal() && launch::LaunchBackend::detect() == launch::LaunchBackend::Direct
    {
        return launch_in_terminal(entry.launch_command()?);
    }
    launch::launch_desktop(target_id).await
}

fn launch_in_terminal(command: Vec<String>) -> anyhow::Result<LaunchReceipt> {
    let (program, command_arguments) = command
        .split_first()
        .context("desktop application command is empty")?;
    let mut arguments = Vec::with_capacity(command_arguments.len() + 2);
    arguments.extend(["--", program.as_str()]);
    arguments.extend(command_arguments.iter().map(String::as_str));
    launch::spawn("xdg-terminal-exec", arguments)
        .context("start application in the default terminal")
}

async fn launch_action(
    catalog: &Catalog,
    target_id: &str,
    action_id: &str,
) -> anyhow::Result<LaunchReceipt> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    let args = entry.parse_action(action_id)?;
    if launch::LaunchBackend::detect() == launch::LaunchBackend::Uwsm {
        return launch::launch_desktop_action(target_id, action_id).await;
    }
    let (program, arguments) = args
        .split_first()
        .context("desktop action command is empty")?;
    launch::spawn(program, arguments).context("start desktop action")
}

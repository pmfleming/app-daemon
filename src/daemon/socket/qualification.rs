//! Exact service-result limits with frozen synthetic input, not a policy fake.
use super::*;
use crate::{api, catalog::Catalog, daemon::AppDaemon, service::ApplicationService};
use anyhow::{Context, ensure};
use serde_json::json;
use shelllist_daemon_tokio::{OwnedTaskRegistry, ServiceBoundary};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::oneshot,
    time::{Duration, timeout},
};

#[path = "../../../tests/support/private_bus.rs"]
mod private_bus;

const DEADLINE: Duration = Duration::from_secs(30);

async fn raw_call(proxy: &zbus::Proxy<'_>, params: &Value) -> Result<String> {
    Ok(timeout(
        DEADLINE,
        proxy.call("Call", &("applications.query", params.to_string())),
    )
    .await??)
}

fn request(params: Value) -> Value {
    json!({"id":"maximum","op":"call","method":"applications.query","params":params,
        "route":{"consumerId":"fixture","localId":"read","kind":"call","generation":1}})
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    timeout(DEADLINE, writer.write_all(format!("{value}\n").as_bytes())).await??;
    Ok(())
}

async fn record(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    request: &Value,
) -> Result<Value> {
    let mut line = String::new();
    ensure!(
        timeout(DEADLINE, reader.read_line(&mut line)).await?? > 0,
        "fixture output EOF"
    );
    let value: Value = serde_json::from_str(&line)?;
    ensure!(
        value["kind"] == "response" && value["id"] == request["id"],
        "fixture correlation"
    );
    ensure!(value["route"] == request["route"], "fixture route");
    let mut expected =
        shelllist_daemon_core::response_message("maximum", value["response"].clone());
    expected["route"] = request["route"].clone();
    ensure!(
        line.len() == serde_json::to_vec(&expected)?.len() + 1,
        "wire envelope/LF size"
    );
    Ok(value["response"].clone())
}

#[tokio::test]
#[ignore = "explicit private exact-result qualification; requires freshly built SHELLLIST_APP_BRIDGE"]
async fn exact_result_limit_and_one_byte_over_on_socket_bridge_and_dbus() -> Result<()> {
    let bridge_path = std::env::var_os("SHELLLIST_APP_BRIDGE")
        .context("SHELLLIST_APP_BRIDGE must name a matching freshly rebuilt app-daemon")?;
    let root = tempfile::Builder::new()
        .prefix("ad-max-")
        .tempdir_in("/tmp")?;
    let catalog_dir = root.path().join("catalog");
    std::fs::create_dir(&catalog_dir)?;
    let entry = "[Desktop Entry]\nType=Application\nName=Large fixture\nExec=true\nComment=";
    let path = catalog_dir.join("large.desktop");
    std::fs::write(&path, format!("{entry}\n"))?;
    let catalog = || Catalog::from_paths(vec![catalog_dir.clone()]);
    let applications = ApplicationService::with_fixture_catalog(catalog()).await;
    let params = json!({"query":"large.desktop","limit":1,"generation":0});
    let baseline = api::ApiService::new(applications.clone())
        .dispatch_owned("applications.query", params.clone(), Some("fixture".into()))
        .await;
    applications.shutdown().await;
    let limit = crate::protocol::methods::Method::parse("applications.query")
        .context("query method")?
        .policy()
        .result_bytes;
    let bytes = limit - serde_json::to_vec(&baseline)?.len() - 10;
    let comment = "é🦀".repeat(bytes / 6) + &"x".repeat(bytes % 6);
    std::fs::write(&path, format!("{entry}{comment}\n"))?;
    let applications = ApplicationService::with_fixture_catalog(catalog()).await;
    let subscriptions = Arc::new(OwnedTaskRegistry::default());
    let daemon = AppDaemon {
        boundary: ServiceBoundary::new(shelllist_daemon_core::ApiIdentity::new(
            api::PROTOCOL,
            api::VERSION as u32,
        )),
        api: Arc::new(api::ApiService::new(applications.clone())),
        applications: applications.clone(),
        subscriptions: subscriptions.clone(),
    };
    let (mut bus, address) = private_bus::start("dbus-daemon", root.path(), DEADLINE).await?;
    let service = zbus::connection::Builder::address(address.as_str())?
        .name(api::BUS_NAME)?
        .serve_at(api::OBJECT_PATH, daemon)?
        .build()
        .await?;
    let client = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let proxy = zbus::Proxy::new(&client, api::BUS_NAME, api::OBJECT_PATH, api::INTERFACE).await?;
    let listener = JsonlListener::bind(&root.path().join("run"), "app-daemon").await?;
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(listener.serve(
        Arc::new(Service(service)),
        crate::client::AppCorrelation,
        JsonlSocketConfig::default(),
        async {
            let _ = stopped.await;
        },
    ));
    let mut socket =
        BufReader::new(UnixStream::connect(root.path().join("run/app-daemon.sock")).await?);
    let mut bridge = Command::new(bridge_path)
        .arg("client")
        .env_clear()
        .env("HOME", root.path())
        .env("TOKIO_WORKER_THREADS", "2")
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent-system-bus",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut bridge_input = bridge.stdin.take().context("bridge stdin")?;
    let mut bridge_output = BufReader::new(bridge.stdout.take().context("bridge stdout")?);
    let near: Value = serde_json::from_str(&raw_call(&proxy, &params).await?)?;
    ensure!(near["ok"] == true, "near limit read rejected");
    let gap = limit
        .checked_sub(serde_json::to_vec(&near)?.len())
        .context("fixture over limit")?;
    ensure!(
        (1..=18).contains(&gap),
        "unexpected opaque revision widths: {gap}"
    );
    // Only an echoed decimal generation changes; the real service data is frozen.
    let generation = 10u64.pow(gap as u32);
    let exact_params = json!({"query":"large.desktop","limit":1,"generation":generation});
    let exact_json = raw_call(&proxy, &exact_params).await?;
    ensure!(
        exact_json.len() == limit,
        "not exact admitted result ceiling"
    );
    let exact: Value = serde_json::from_str(&exact_json)?;
    ensure!(exact["ok"] == true, "exact service result rejected");
    ensure!(
        exact["data"]["applications"]["applications"][0]["comment"] == comment,
        "UTF-8 data changed"
    );
    let call = request(exact_params);
    send(socket.get_mut(), &call).await?;
    ensure!(
        record(&mut socket, &call).await? == exact,
        "maximum socket parity"
    );
    send(&mut bridge_input, &call).await?;
    ensure!(
        record(&mut bridge_output, &call).await? == exact,
        "maximum retained bridge parity"
    );
    let over_params = json!({"query":"large.desktop","limit":1,"generation":generation * 10});
    let mut over = exact;
    over["data"]["applications"]["generation"] = json!(generation * 10);
    ensure!(
        serde_json::to_vec(&over)?.len() == limit + 1,
        "not exactly one byte over"
    );
    let rejected: Value = serde_json::from_str(&raw_call(&proxy, &over_params).await?)?;
    ensure!(
        rejected["ok"] == false && rejected["error"]["code"] == "outcome-unknown",
        "oversized result not rejected"
    );
    ensure!(
        rejected["error"]["retryable"] == false,
        "no replay permission on missing result"
    );
    let call = request(over_params);
    send(socket.get_mut(), &call).await?;
    ensure!(
        record(&mut socket, &call).await? == rejected,
        "oversized socket parity"
    );
    send(&mut bridge_input, &call).await?;
    ensure!(
        record(&mut bridge_output, &call).await? == rejected,
        "oversized bridge parity"
    );
    // Neither service-side result rejection nor the native socket retires a peer.
    let call = request(params);
    send(socket.get_mut(), &call).await?;
    ensure!(
        record(&mut socket, &call).await?["ok"] == true,
        "socket did not recover"
    );
    send(&mut bridge_input, &call).await?;
    ensure!(
        record(&mut bridge_output, &call).await?["ok"] == true,
        "bridge did not recover"
    );
    send(&mut bridge_input, &json!({"id":"shutdown","op":"shutdown"})).await?;
    ensure!(
        timeout(DEADLINE, bridge.wait()).await??.success(),
        "bridge shutdown"
    );
    drop(socket);
    let _ = stop.send(());
    timeout(DEADLINE, task).await???;
    subscriptions.shutdown().await;
    applications.shutdown().await;
    bus.kill().await?;
    Ok(())
}

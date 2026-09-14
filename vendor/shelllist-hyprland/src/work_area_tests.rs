use super::work_area::*;
use serde_json::{Value, json};

fn fixture() -> [Value; 5] {
    [
        json!([
            {"id":0,"name":"eDP-1","description":"Laptop panel","focused":true,"x":0,"y":0,"width":1920,"height":1200,"scale":1.25,"activeWorkspace":{"id":1},"specialWorkspace":{"id":0},"reserved":[0,51,0,0]},
            {"id":1,"name":"DP-1","description":"External display","make":"Acme","model":"Panel","serial":"123","x":1536,"y":0,"width":3840,"height":2160,"scale":2,"activeWorkspace":{"id":3},"reserved":[24,80,10,20]}
        ]),
        json!([{"id":1,"name":"1","windows":2},{"id":3,"name":"3","windows":0}]),
        json!([]),
        json!([{"workspace":{"id":1},"mapped":true,"floating":false,"grouped":["a","b"]},{"workspace":{"id":1},"mapped":true,"floating":true}]),
        json!({"css":"2"}),
    ]
}
fn text(parts: &[Value; 5]) -> String {
    parts
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n\n")
}
fn margins(parts: &[Value; 5], monitor: &str) -> Value {
    serde_json::to_value(parse_work_areas(&text(parts)).unwrap().get(monitor)).unwrap()
}
#[test]
fn logical_reservations_css_gaps_and_ordered_rules() {
    let mut parts = fixture();
    assert_eq!(
        margins(&parts, "eDP-1"),
        json!({"left":2.0,"top":53.0,"right":2.0,"bottom":2.0})
    );
    parts[2] = json!([{"workspaceString":"r[1-4]","gapsOut":[9,10,11,12]},{"workspaceString":"1","gapsOut":[3,4,5,6]},{"workspaceString":"1","borderSize":10}]);
    assert_eq!(
        margins(&parts, "eDP-1"),
        json!({"left":6.0,"top":54.0,"right":4.0,"bottom":5.0})
    );
    assert_eq!(
        margins(&parts, "DP-1"),
        json!({"left":36.0,"top":89.0,"right":20.0,"bottom":31.0})
    );
    parts[2] = json!([]);
    parts[4]["css"] = json!("4 8 12 16");
    parts[0][0]["reserved"] = json!([7, 92, 13, 28]);
    assert_eq!(
        margins(&parts, "eDP-1"),
        json!({"left":23.0,"top":96.0,"right":21.0,"bottom":40.0})
    );
    assert!(margins(&parts, "missing").is_null());
}
#[test]
fn smart_gaps_and_group_visibility_filters() {
    let mut parts = fixture();
    for selector in [
        "m[eDP-1] w[t1]",
        "w[tg1]",
        "w[f1]",
        "r[1-4] m[current] w[2] f[-1]",
    ] {
        parts[2] = json!([{"workspaceString":selector,"gapsOut":[0,0,0,0]}]);
        assert_eq!(margins(&parts, "eDP-1")["top"], 51.0, "{selector}");
    }
    parts[2] = json!([{"workspaceString":"w[t1]","gapsOut":[0,0,0,0]}]);
    parts[3][1]["floating"] = json!(false);
    assert_eq!(margins(&parts, "eDP-1")["top"], 53.0);
    parts[3][1]["grouped"] = json!(["b", "a"]);
    parts[2] = json!([{"workspaceString":"w[tg1]","gapsOut":[0,0,0,0]}]);
    assert_eq!(margins(&parts, "eDP-1")["top"], 51.0);
    parts[3][1]["floating"] = json!(true);
    parts[3][1]["pinned"] = json!(true);
    parts[3][1]["visible"] = json!(false);
    for selector in ["w[p1]", "w[fv0]"] {
        parts[2] = json!([{"workspaceString":selector,"gapsOut":[0,0,0,0]}]);
        assert_eq!(margins(&parts, "eDP-1")["top"], 51.0);
    }
}
#[test]
fn named_special_monitor_and_malformed_selectors() {
    let mut parts = fixture();
    for selector in [
        "m[+1]",
        "m[-1]",
        "m[+3]",
        "m[r]",
        "m[1]",
        "m[desc:Acme Panel]",
    ] {
        parts[2] = json!([{"workspaceString":selector,"gapsOut":[0,0,0,0]}]);
        assert_eq!(margins(&parts, "DP-1")["top"], 80.0, "{selector}");
    }
    parts[2] = json!([{"workspaceString":"m[r]","gapsOut":[0,0,0,0]}]);
    parts[0][1]["x"] = json!(1538);
    assert_eq!(margins(&parts, "DP-1")["top"], 82.0);
    parts[0][1]["x"] = json!(1537);
    assert_eq!(margins(&parts, "DP-1")["top"], 80.0);
    for selector in ["r[1-4] junk", "w[bad]", "n[true]", "m[right]", ""] {
        if selector.is_empty() {
            continue;
        }
        parts[2] = json!([{"workspaceString":selector,"gapsOut":[0,0,0,0]}]);
        assert_eq!(margins(&parts, "eDP-1")["top"], 53.0);
    }
    parts[1]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":-1337,"name":"project:web","windows":0}));
    parts[0][0]["activeWorkspace"] = json!({"id":-1337});
    parts[2] = json!([{"workspaceString":"n[true] n[s:project:] n[e:web]","gapsOut":[1,2,3,4]},{"workspaceString":"name:project:web","gapsOut":[5,6,7,8]}]);
    assert_eq!(margins(&parts, "eDP-1")["top"], 56.0);
    parts[1]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":-99,"name":"special:notes"}));
    parts[0][0]["specialWorkspace"] = json!({"id":-99});
    parts[2] = json!([{"workspaceString":"s[true]","gapsOut":[0,0,0,0]}]);
    assert_eq!(margins(&parts, "eDP-1")["top"], 51.0);
}
#[tokio::test]
#[ignore = "requires a running Hyprland session; read-only probe"]
async fn live_work_areas() {
    let areas = crate::Client::default().work_areas().await.unwrap();
    assert!(!areas.is_empty());
    assert!(areas.values().all(|value| {
        [value.left, value.top, value.right, value.bottom]
            .iter()
            .all(|v| v.is_finite())
    }));
}

#[tokio::test]
async fn native_batch_request_returns_normalized_work_areas() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().unwrap();
    let instance = root.path().join("hypr/test");
    tokio::fs::create_dir_all(&instance).await.unwrap();
    let listener = tokio::net::UnixListener::bind(instance.join(".socket.sock")).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut command = String::new();
        stream.read_to_string(&mut command).await.unwrap();
        assert_eq!(
            command,
            "[[BATCH]]j/monitors;j/workspaces;j/workspacerules;j/clients;j/getoption general:gaps_out"
        );
        stream.write_all(text(&fixture()).as_bytes()).await.unwrap();
    });
    let client = crate::Client::new(root.path().into(), Some("test".into()));
    assert_eq!(client.work_areas().await.unwrap()["eDP-1"].top, 53.0);
    server.await.unwrap();
}

#[test]
fn parses_adjacent_json_and_rejects_missing_or_invalid_documents() {
    let mut parts = fixture();
    parts[0][0]["description"] = json!("Display with } [ \" \\ and\nnewline");
    assert!(parse_work_areas(&text(&parts)).is_ok());
    for invalid in [
        "",
        "not running",
        "[] [] [] [] {}",
        "[null] [] [] [] {\"css\":\"2\"}",
    ] {
        assert!(parse_work_areas(invalid).is_err());
    }
    parts[0][0]["scale"] = json!(0);
    assert!(parse_work_areas(&text(&parts)).is_err());
    assert!(geometry_event("configreloaded>>"));
    assert!(geometry_event("togglegroup>>1"));
    assert!(!geometry_event("activewindow>>title"));
}

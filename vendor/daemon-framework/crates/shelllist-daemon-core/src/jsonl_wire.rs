use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ClientRequest {
    Call {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Subscribe {
        id: String,
        #[serde(default)]
        streams: Vec<String>,
    },
    Cancel {
        id: String,
        request_id: String,
    },
    Shutdown {
        id: String,
    },
}

#[must_use]
pub fn response_message(id: &str, response: Value) -> Value {
    json!({ "kind": "response", "id": id, "ok": true, "response": response })
}

#[must_use]
pub fn response_error_message(id: &str, error: impl Into<String>) -> Value {
    json!({ "kind": "response", "id": id, "ok": false, "error": error.into() })
}

#[must_use]
pub fn event_message(stream: &str, event: Value) -> Value {
    json!({ "kind": "event", "stream": stream, "event": event })
}

#[must_use]
pub fn protocol_error_message(error: impl Into<String>) -> Value {
    json!({ "kind": "protocol-error", "error": error.into() })
}

#[must_use]
pub fn transport_error_message(error: impl Into<String>) -> Value {
    json!({ "kind": "transport-error", "error": error.into() })
}

#[must_use]
pub fn shutdown_message(id: &str) -> Value {
    response_message(id, json!({ "shutdown": true }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ClientRequest, shutdown_message};

    #[test]
    fn request_defaults_and_wire_names_are_stable() -> serde_json::Result<()> {
        assert_eq!(
            serde_json::from_str::<ClientRequest>(
                r#"{"op":"call","id":"1","method":"things.get"}"#
            )?,
            ClientRequest::Call {
                id: "1".into(),
                method: "things.get".into(),
                params: Value::Null,
            }
        );
        assert_eq!(
            shutdown_message("bye"),
            json!({
                "kind": "response", "id": "bye", "ok": true,
                "response": { "shutdown": true }
            })
        );
        Ok(())
    }
}

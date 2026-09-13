use serde::{Deserialize, Serialize};
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
    /// Release subscriptions belonging to the routed consumer. In-flight
    /// replies retain their route so a detached frontend can cancel late IDs.
    Release {
        id: String,
    },
    Shutdown {
        id: String,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RouteKind {
    Call,
    Subscription,
    BaseSubscription,
    Control,
}

/// Bridge-local addressing, never forwarded into the domain D-Bus API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientRoute {
    pub consumer_id: String,
    pub local_id: String,
    pub generation: u64,
    pub kind: RouteKind,
}

/// Unrouted legacy clients keep their existing wire format.
#[derive(Debug, Deserialize, PartialEq)]
pub struct ClientMessage {
    #[serde(flatten)]
    pub request: ClientRequest,
    #[serde(default)]
    pub route: Option<ClientRoute>,
}

impl ClientMessage {
    pub fn validate(&self) -> Result<(), &'static str> {
        let Some(route) = &self.route else {
            return if matches!(self.request, ClientRequest::Release { .. }) {
                Err("release requires a consumer route")
            } else {
                Ok(())
            };
        };
        if route.consumer_id.is_empty()
            || route.consumer_id.len() > 256
            || route.local_id.is_empty()
            || route.local_id.len() > 1024
        {
            return Err("invalid bridge route identifier length");
        }
        let valid = matches!(
            (&self.request, route.kind),
            (ClientRequest::Call { .. }, RouteKind::Call)
                | (
                    ClientRequest::Subscribe { .. },
                    RouteKind::Subscription | RouteKind::BaseSubscription
                )
                | (
                    ClientRequest::Cancel { .. } | ClientRequest::Release { .. },
                    RouteKind::Control
                )
        );
        if valid {
            Ok(())
        } else {
            Err("bridge route kind does not match request")
        }
    }
}

#[must_use]
pub fn addressed_message(mut message: Value, route: Option<&ClientRoute>) -> Value {
    if let Some(route) = route {
        message["route"] = json!(route);
    }
    message
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

    use super::{
        ClientMessage, ClientRequest, RouteKind, addressed_message, response_message,
        shutdown_message,
    };

    #[test]
    fn routing_is_optional_typed_and_bridge_local() -> serde_json::Result<()> {
        let legacy: ClientMessage = serde_json::from_value(json!({
            "op": "call", "id": "1", "method": "things.get"
        }))?;
        legacy.validate().unwrap();
        assert!(legacy.route.is_none());
        let route =
            json!({ "consumerId": "view", "localId": "page", "kind": "call", "generation": 4 });
        let message: ClientMessage = serde_json::from_value(json!({
            "op": "call", "id": "view::page", "method": "things.get", "route": route
        }))?;
        message.validate().unwrap();
        assert_eq!(message.route.as_ref().unwrap().kind, RouteKind::Call);
        let reply = response_message("view::page", json!({ "data": {} }));
        assert!(
            addressed_message(reply.clone(), None)
                .get("route")
                .is_none()
        );
        assert_eq!(
            addressed_message(reply, message.route.as_ref())["route"],
            route
        );
        Ok(())
    }

    #[test]
    fn rejects_mismatched_or_unbounded_route_metadata() -> serde_json::Result<()> {
        for (op, kind, consumer) in [
            ("call", "control", "view".into()),
            ("subscribe", "call", "view".into()),
            ("release", "call", "view".into()),
            ("call", "call", "".into()),
            ("call", "call", "v".repeat(257)),
        ] {
            let message: ClientMessage = serde_json::from_value(json!({
                "op": op, "id": "1", "method": "things.get",
                "route": { "consumerId": consumer, "localId": "1", "generation": 0, "kind": kind }
            }))?;
            assert!(message.validate().is_err());
        }
        let release: ClientMessage = serde_json::from_value(json!({ "op": "release", "id": "1" }))?;
        assert!(release.validate().is_err());
        Ok(())
    }

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

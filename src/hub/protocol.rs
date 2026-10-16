//! The messages a connector and the hub exchange as JSON text frames over `/connect`.

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};

/// A message from a connector to the hub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorMessage {
    /// The connector's current services, replacing any it registered before.
    Register {
        machine: String,
        services: Vec<RegisteredService>,
    },
    Result {
        id: u64,
        result: CallToolResult,
    },
    Error {
        id: u64,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisteredService {
    pub name: String,
    pub tools: Vec<Tool>,
    /// Set only for http services, which agents' own servers are matched against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A message from the hub to a connector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HubMessage {
    Call {
        id: u64,
        service: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<JsonObject>,
    },
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn round_trip<T: Serialize + DeserializeOwned>(json: Value) -> T {
        let message: T = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(serde_json::to_value(&message).unwrap(), json);

        message
    }

    #[test]
    fn round_trips_register() {
        let message = round_trip(json!({
            "type": "register",
            "machine": "laptop",
            "services": [
                {
                    "name": "echo",
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes text",
                        "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
                    }],
                },
                { "name": "remote", "tools": [], "url": "https://example.com/mcp" },
            ],
        }));

        let ConnectorMessage::Register { machine, services } = message else {
            panic!("expected register, got {message:?}");
        };
        assert_eq!(machine, "laptop");
        assert_eq!(services[0].tools[0].name, "echo");
        assert_eq!(services[0].url, None);
        assert_eq!(services[1].url.as_deref(), Some("https://example.com/mcp"));
    }

    #[test]
    fn round_trips_result_and_error() {
        let result = round_trip(json!({
            "type": "result",
            "id": 7,
            "result": { "content": [{ "type": "text", "text": "hi" }], "isError": false },
        }));
        let error: ConnectorMessage =
            round_trip(json!({ "type": "error", "id": 8, "message": "nope" }));

        assert!(matches!(result, ConnectorMessage::Result { id: 7, .. }));
        assert_eq!(
            error,
            ConnectorMessage::Error {
                id: 8,
                message: "nope".to_owned(),
            }
        );
    }

    #[test]
    fn round_trips_call_with_and_without_arguments() {
        let with = round_trip(json!({
            "type": "call",
            "id": 1,
            "service": "echo",
            "tool": "echo",
            "arguments": { "text": "hi" },
        }));
        let without =
            round_trip(json!({ "type": "call", "id": 2, "service": "echo", "tool": "hang" }));

        assert!(
            matches!(with, HubMessage::Call { arguments: Some(arguments), .. } if arguments["text"] == "hi")
        );
        assert!(matches!(
            without,
            HubMessage::Call {
                arguments: None,
                ..
            }
        ));
    }

    #[test]
    fn rejects_unknown_types() {
        let message = json!({ "type": "ping", "id": 1 });

        assert!(serde_json::from_value::<ConnectorMessage>(message.clone()).is_err());
        assert!(serde_json::from_value::<HubMessage>(message).is_err());
    }
}

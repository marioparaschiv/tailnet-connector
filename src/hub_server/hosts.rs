use std::collections::BTreeMap;
use std::fmt;

use rmcp::model::{CallToolResult, JsonObject};
use tokio::sync::{mpsc, oneshot, watch};

use crate::hub::protocol::RegisteredService;

/// Every connected host by machine name.
pub type HostMap = BTreeMap<String, Host>;

/// The connected hosts. Clones share one map.
#[derive(Clone, Default)]
pub struct Hosts(watch::Sender<HostMap>);

/// A machine's connector: the services it registered and a way to call them.
#[derive(Debug, Clone)]
pub struct Host {
    pub services: Vec<RegisteredService>,
    pub caller: Caller,
}

/// Sends calls to one connector. Clones send to the same connector.
#[derive(Debug, Clone)]
pub struct Caller(mpsc::UnboundedSender<Call>);

/// A tool call for a connector to run, answered through `reply` with the result or the
/// connector's error message.
#[derive(Debug)]
pub struct Call {
    pub service: String,
    pub tool: String,
    pub arguments: Option<JsonObject>,
    pub reply: oneshot::Sender<Result<CallToolResult, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The connector answered with this message.
    Connector(String),
    /// The connector went away before answering.
    Disconnected,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connector(message) => f.write_str(message),
            Self::Disconnected => f.write_str("connector disconnected"),
        }
    }
}

impl std::error::Error for CallError {}

impl Hosts {
    /// Adds `host` under `machine`, replacing any host already there.
    pub fn insert(&self, machine: String, host: Host) {
        self.0.send_modify(|hosts| {
            hosts.insert(machine, host);
        });
    }

    /// Removes `machine` only while its host still calls through `caller`, so a socket closing
    /// after its machine reconnected leaves the new host in place. Returns whether it removed it.
    pub fn remove_if_same(&self, machine: &str, caller: &Caller) -> bool {
        self.0.send_if_modified(|hosts| {
            if !hosts
                .get(machine)
                .is_some_and(|host| host.caller.0.same_channel(&caller.0))
            {
                return false;
            }

            hosts.remove(machine);

            true
        })
    }

    pub fn snapshot(&self) -> HostMap {
        self.0.borrow().clone()
    }

    /// Marked changed by every insert and every removal.
    pub fn subscribe(&self) -> watch::Receiver<HostMap> {
        self.0.subscribe()
    }
}

impl Caller {
    /// A caller and the calls sent through it. Whoever owns the receiver is the connector: the
    /// `/connect` socket in the hub, or a test answering calls itself.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Call>) {
        let (calls, receiver) = mpsc::unbounded_channel();

        (Self(calls), receiver)
    }

    pub async fn call(
        &self,
        service: String,
        tool: String,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, CallError> {
        let (reply, result) = oneshot::channel();
        let call = Call {
            service,
            tool,
            arguments,
            reply,
        };
        self.0.send(call).map_err(|_| CallError::Disconnected)?;

        result
            .await
            .map_err(|_| CallError::Disconnected)?
            .map_err(CallError::Connector)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ContentBlock;

    use super::*;

    fn host(service: &str) -> (Host, mpsc::UnboundedReceiver<Call>) {
        let (caller, calls) = Caller::channel();
        let services = vec![RegisteredService {
            name: service.to_owned(),
            tools: Vec::new(),
            url: None,
        }];

        (Host { services, caller }, calls)
    }

    fn service_names(hosts: &Hosts) -> Vec<(String, String)> {
        hosts
            .snapshot()
            .into_iter()
            .flat_map(|(machine, host)| {
                host.services
                    .into_iter()
                    .map(move |service| (machine.clone(), service.name))
            })
            .collect()
    }

    fn pair(machine: &str, service: &str) -> (String, String) {
        (machine.to_owned(), service.to_owned())
    }

    #[test]
    fn inserts_and_replaces_hosts() {
        let hosts = Hosts::default();
        let (laptop, _laptop_calls) = host("echo");
        let (desktop, _desktop_calls) = host("ghidra");
        let (reconnected, _reconnected_calls) = host("aside");

        hosts.insert("laptop".to_owned(), laptop);
        hosts.insert("desktop".to_owned(), desktop);
        hosts.insert("laptop".to_owned(), reconnected);

        assert_eq!(
            service_names(&hosts),
            [pair("desktop", "ghidra"), pair("laptop", "aside")]
        );
    }

    #[test]
    fn removes_a_host_only_through_its_own_caller() {
        let hosts = Hosts::default();
        let (old, _old_calls) = host("echo");
        let (new, _new_calls) = host("echo");
        let old_caller = old.caller.clone();
        let new_caller = new.caller.clone();

        hosts.insert("laptop".to_owned(), old);
        hosts.insert("laptop".to_owned(), new);

        assert!(!hosts.remove_if_same("laptop", &old_caller));
        assert_eq!(service_names(&hosts), [pair("laptop", "echo")]);
        assert!(!hosts.remove_if_same("desktop", &new_caller));
        assert!(hosts.remove_if_same("laptop", &new_caller));
        assert!(hosts.snapshot().is_empty());
    }

    #[test]
    fn signals_inserts_and_removals_but_not_removals_that_did_nothing() {
        let hosts = Hosts::default();
        let mut changes = hosts.subscribe();
        let (laptop, _calls) = host("echo");
        let (other, _other_calls) = host("echo");
        let caller = laptop.caller.clone();

        hosts.insert("laptop".to_owned(), laptop);
        assert!(changes.has_changed().unwrap());
        assert!(changes.borrow_and_update().contains_key("laptop"));

        hosts.remove_if_same("laptop", &other.caller);
        assert!(!changes.has_changed().unwrap());

        hosts.remove_if_same("laptop", &caller);
        assert!(changes.has_changed().unwrap());
        assert!(changes.borrow_and_update().is_empty());
    }

    #[tokio::test]
    async fn calls_reach_whoever_answers_them() {
        let (caller, mut calls) = Caller::channel();
        let connector = tokio::spawn(async move {
            while let Some(call) = calls.recv().await {
                let reply = match call.tool.as_str() {
                    "echo" => Ok(CallToolResult::success(vec![ContentBlock::text(
                        call.arguments.unwrap()["text"].as_str().unwrap(),
                    )])),
                    _ => Err(format!("{} has no tool {}", call.service, call.tool)),
                };
                call.reply.send(reply).unwrap();
            }
        });

        let arguments = serde_json::json!({ "text": "hi" });
        let result = caller
            .call(
                "echo".to_owned(),
                "echo".to_owned(),
                arguments.as_object().cloned(),
            )
            .await
            .unwrap();
        let error = caller
            .call("echo".to_owned(), "missing".to_owned(), None)
            .await
            .unwrap_err();

        assert_eq!(result.content[0].as_text().unwrap().text, "hi");
        assert_eq!(
            error,
            CallError::Connector("echo has no tool missing".to_owned())
        );

        drop(caller);
        connector.await.unwrap();
    }

    #[tokio::test]
    async fn fails_calls_when_the_connector_goes_away() {
        let (caller, mut calls) = Caller::channel();
        let pending = tokio::spawn({
            let caller = caller.clone();
            async move {
                caller
                    .call("echo".to_owned(), "hang".to_owned(), None)
                    .await
            }
        });

        let call = calls.recv().await.unwrap();
        drop(call);
        assert_eq!(pending.await.unwrap(), Err(CallError::Disconnected));

        drop(calls);
        assert_eq!(
            caller
                .call("echo".to_owned(), "echo".to_owned(), None)
                .await,
            Err(CallError::Disconnected)
        );
    }
}

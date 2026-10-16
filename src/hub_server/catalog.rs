//! Every connected host's tools as agents see them, named `<machine>__<service>__<tool>`.

use std::sync::{Arc, Mutex};

use rmcp::model::Tool;
use tokio::sync::watch;
use url::Url;

use crate::hub_server::hosts::{Caller, HostMap, Hosts};
use crate::known_servers::KnownServer;

/// One host tool under its catalog name.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The upstream tool renamed and with its description prefixed by machine and service.
    pub tool: Tool,
    pub machine: String,
    pub service: String,
    /// Normalised, and set only for http services.
    pub service_url: Option<String>,
    pub upstream_name: String,
    pub caller: Caller,
}

#[derive(Debug, Default)]
pub struct Catalog(Vec<Entry>);

impl Catalog {
    /// Skips, with a log line, any tool whose catalog name is not a valid MCP tool name.
    pub fn build(hosts: &HostMap) -> Self {
        let mut entries = Vec::new();

        for (machine, host) in hosts {
            for service in &host.services {
                for upstream in &service.tools {
                    let name = format!("{machine}__{}__{}", service.name, upstream.name);

                    if !is_tool_name(&name) {
                        eprintln!(
                            "skipping {name}: not a valid MCP tool name (max 64 chars, [A-Za-z0-9_-])"
                        );

                        continue;
                    }

                    let mut tool = upstream.clone();
                    tool.description = Some(
                        format!(
                            "[{machine} / {}] {}",
                            service.name,
                            upstream.description.as_deref().unwrap_or_default()
                        )
                        .into(),
                    );
                    tool.name = name.into();

                    entries.push(Entry {
                        tool,
                        machine: machine.clone(),
                        service: service.name.clone(),
                        service_url: service.url.as_deref().map(normalise_service_url),
                        upstream_name: upstream.name.to_string(),
                        caller: host.caller.clone(),
                    });
                }
            }
        }

        Self(entries)
    }

    /// The entries an agent with `known` servers sees: its own servers win over the hub's copies.
    pub fn visible<'a>(&'a self, known: &'a [KnownServer]) -> impl Iterator<Item = &'a Entry> {
        self.0.iter().filter(|entry| !is_known(known, entry))
    }
}

/// `^[A-Za-z0-9_-]{1,64}$`
fn is_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

// Http servers match by URL, since one name can point at different servers; stdio servers match by
// name.
fn is_known(known: &[KnownServer], entry: &Entry) -> bool {
    known.iter().any(|server| {
        server.url == entry.service_url
            && (entry.service_url.is_some() || server.name == entry.service)
    })
}

/// The WHATWG `href` of `url`, so that equivalent spellings of one server's URL compare equal.
pub fn normalise_url(url: &str) -> Result<String, url::ParseError> {
    Url::parse(url).map(String::from)
}

// `/connect` only accepts registrations whose URLs parse.
fn normalise_service_url(url: &str) -> String {
    normalise_url(url).unwrap_or_else(|_| url.to_owned())
}

/// The catalog of the current hosts, rebuilt on first use after they change. Clones share it.
#[derive(Clone)]
pub struct Catalogs(Arc<Mutex<(watch::Receiver<HostMap>, Arc<Catalog>)>>);

impl Catalogs {
    pub fn new(hosts: &Hosts) -> Self {
        let mut changes = hosts.subscribe();
        let catalog = Catalog::build(&changes.borrow_and_update());

        Self(Arc::new(Mutex::new((changes, Arc::new(catalog)))))
    }

    pub fn current(&self) -> Arc<Catalog> {
        let mut cache = self.0.lock().expect("no catalog build panics");
        let (changes, catalog) = &mut *cache;

        // An error means every `Hosts` is gone, so the hosts can no longer change.
        if changes.has_changed().unwrap_or(false) {
            *catalog = Arc::new(Catalog::build(&changes.borrow_and_update()));
        }

        Arc::clone(catalog)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::JsonObject;

    use super::*;
    use crate::hub::protocol::RegisteredService;
    use crate::hub_server::hosts::Host;

    fn tool(name: &str) -> Tool {
        Tool::new(name.to_owned(), format!("{name} things"), JsonObject::new())
    }

    fn service(name: &str, url: Option<&str>, tools: &[&str]) -> RegisteredService {
        RegisteredService {
            name: name.to_owned(),
            tools: tools.iter().map(|name| tool(name)).collect(),
            url: url.map(str::to_owned),
        }
    }

    fn hosts(services: Vec<RegisteredService>) -> HostMap {
        let (caller, _calls) = Caller::channel();

        HostMap::from([("laptop".to_owned(), Host { services, caller })])
    }

    fn known(name: &str, url: Option<&str>) -> KnownServer {
        KnownServer {
            name: name.to_owned(),
            url: url.map(|url| normalise_url(url).unwrap()),
        }
    }

    fn visible_names(catalog: &Catalog, known: &[KnownServer]) -> Vec<String> {
        catalog
            .visible(known)
            .map(|entry| entry.tool.name.to_string())
            .collect()
    }

    #[test]
    fn names_and_prefixes_tools_and_skips_invalid_names() {
        // `laptop__echo__` leaves 50 characters for the tool.
        let long = "x".repeat(51);
        let catalog = Catalog::build(&hosts(vec![
            service("echo", None, &["echo", "has space", &long]),
            service("bad.service", None, &["echo"]),
        ]));

        assert_eq!(visible_names(&catalog, &[]), ["laptop__echo__echo"]);

        let entry = &catalog.0[0];
        assert_eq!(
            entry.tool.description.as_deref(),
            Some("[laptop / echo] echo things")
        );
        assert_eq!(
            (entry.machine.as_str(), entry.service.as_str()),
            ("laptop", "echo")
        );
        assert_eq!(entry.upstream_name, "echo");
    }

    #[test]
    fn checks_tool_names_like_the_mcp_pattern() {
        for valid in ["a", "A-z_0", &"x".repeat(64)] {
            assert!(is_tool_name(valid), "{valid}");
        }

        for invalid in ["", "a.b", "a b", "é", &"x".repeat(65)] {
            assert!(!is_tool_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn hides_services_the_agent_already_has() {
        let catalog = Catalog::build(&hosts(vec![
            service("aside", None, &["exec"]),
            service(
                "linear",
                Some("https://MCP.linear.app:443/mcp"),
                &["search"],
            ),
            service("github", Some("https://github.example/mcp"), &["issues"]),
            service("chrome", None, &["click"]),
        ]));
        let known = [
            known("aside", None),
            known("my-linear", Some("https://mcp.linear.app/mcp")),
            known("github", Some("https://other.example/mcp")),
            known("chrome", Some("https://chrome.example/mcp")),
        ];

        assert_eq!(
            visible_names(&catalog, &known),
            ["laptop__github__issues", "laptop__chrome__click"]
        );
    }

    #[test]
    fn rebuilds_after_hosts_change() {
        let hosts = Hosts::default();
        let catalogs = Catalogs::new(&hosts);
        assert!(visible_names(&catalogs.current(), &[]).is_empty());

        let (caller, _calls) = Caller::channel();
        let services = vec![service("echo", None, &["echo"])];
        hosts.insert("laptop".to_owned(), Host { services, caller });

        assert_eq!(
            visible_names(&catalogs.current(), &[]),
            ["laptop__echo__echo"]
        );
    }
}

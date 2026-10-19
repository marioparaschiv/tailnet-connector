use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::Arc;

use futures_util::future::join_all;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::checks;
use crate::config::{Config, ConfigError};
use crate::hub::{self, Registration};
use crate::reload::{self, Watcher};
use crate::services::Services;

pub struct Connector {
    config: Config,
    watcher: Watcher,
    registration: watch::Sender<Registration>,
}

impl Connector {
    /// Lists the configured services and checks which are available, failing on unreadable
    /// config or checks files.
    pub async fn start(config: Config) -> Result<Self, ConfigError> {
        let watcher = Watcher::new(config.claude_config.clone(), config.pi_config.clone())?;
        let services = Services::start(watcher.mcp_servers(), &config).await;
        let connector = Self {
            config,
            watcher,
            registration: watch::Sender::new(Registration {
                services: Arc::new(services),
                available: BTreeSet::new(),
            }),
        };
        connector.refresh_availability().await?;

        Ok(connector)
    }

    /// Serves the hub forever while rechecking availability and watching the MCP configs.
    pub async fn run(mut self) -> Infallible {
        let config = self.config.clone();

        tokio::select! {
            never = hub::run(&config, self.registration.subscribe()) => never,
            never = self.maintain() => never,
        }
    }

    async fn maintain(&mut self) -> Infallible {
        let mut checks = interval_at(
            Instant::now() + self.config.check_interval,
            self.config.check_interval,
        );
        let mut reload = interval_at(Instant::now() + reload::POLL, reload::POLL);
        checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        reload.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = checks.tick() => {
                    if let Err(error) = self.refresh_availability().await {
                        eprintln!("availability check failed: {error}");
                    }
                }
                _ = reload.tick() => {
                    if let Err(error) = self.reload_if_changed().await {
                        eprintln!("cannot reload the MCP config: {error}");
                    }
                }
            }
        }
    }

    async fn refresh_availability(&self) -> Result<(), ConfigError> {
        let services = Arc::clone(&self.registration.borrow().services);
        let available = self.check(&services).await?;

        if available == self.registration.borrow().available {
            return Ok(());
        }

        self.publish(services, available).await;

        Ok(())
    }

    async fn reload_if_changed(&mut self) -> Result<(), ConfigError> {
        let Some(mcp_servers) = self.watcher.changed()? else {
            return Ok(());
        };

        let previous = Arc::clone(&self.registration.borrow().services);
        previous.stop_all().await;

        let services = Arc::new(Services::start(mcp_servers, &self.config).await);
        println!(
            "mcpServers changed: {}",
            services.names().collect::<Vec<_>>().join(", ")
        );

        // The old services are already stopped, so the new ones go out even when the checks
        // file can't be read, with the previously available names until a check succeeds.
        let (available, checked) = match self.check(&services).await {
            Ok(available) => (available, Ok(())),
            Err(error) => (self.registration.borrow().available.clone(), Err(error)),
        };
        self.publish(services, available).await;

        checked
    }

    async fn check(&self, services: &Services) -> Result<BTreeSet<String>, ConfigError> {
        let checks = checks::read(&self.config.checks)?;

        Ok(checks::available(services.names(), &checks).await)
    }

    /// Registers the new state with the hub and stops the servers of services that dropped out.
    async fn publish(&self, services: Arc<Services>, available: BTreeSet<String>) {
        let lost = {
            let previous = self.registration.borrow();

            previous
                .available
                .difference(&available)
                .filter_map(|name| previous.services.get(name).cloned())
                .collect::<Vec<_>>()
        };
        let registration = Registration {
            services,
            available,
        };
        let names = registration.names("none");

        self.registration.send_replace(registration);
        join_all(lost.iter().map(|server| server.stop())).await;
        println!("available: {names}");
    }
}

# Tailnet Connector

Tailnet Connector makes the MCP servers configured on each of your machines available to Claude Code and Pi agents on any other machine in your Tailscale network.

For example, if a laptop runs a browser automation server and a desktop runs a database server, an agent on either machine, or on a third one, can call the tools of both.

## Architecture

A single binary provides three roles, selected by the command used to start it.

1. **Hub.** Runs on one machine. It aggregates the tools registered by every connector and serves them to agents.
2. **Connector.** Runs on each machine that shares MCP servers. It reads the servers defined in `~/.claude.json` and Pi's `~/.pi/agent/mcp.json` and registers their tools with the hub.
3. **Agent proxy.** Runs inside Claude Code or Pi as a stdio MCP server and connects the agent to the hub.

Agents see each remote tool under the name `<machine>__<service>__<tool>`, such as `laptop__browser__screenshot`.

All traffic travels over Tailscale. The hub listens only on its Tailscale address, so it is not reachable from the public internet.

## Requirements

- Tailscale, installed and signed in on every participating machine
- Rust, installed through `rustup`, which fetches the pinned toolchain on the first build
- Claude Code or Pi, on each machine that runs agents

## Setup

### 1. Build the binary

Run the following on each machine:

```sh
cargo build --release
```

The build produces `target/release/tailnet-connector`. Place it on your `PATH`.

### 2. Start the hub

Choose one machine to act as the hub and run:

```sh
tailnet-connector hub
```

On startup, the hub prints the addresses it listens on:

```
tailnet MCP listening on http://100.x.y.z:8790/mcp
connectors attach at ws://100.x.y.z:8790/connect
```

The remaining steps refer to `http://100.x.y.z:8790` as the hub URL.

### 3. Start a connector on each machine

Run a connector on every machine whose MCP servers you want to share. This includes the hub machine if it has servers of its own.

```sh
TAILNET_URL=http://100.x.y.z:8790 tailnet-connector
```

The connector shares every server listed under `mcpServers` in `~/.claude.json` and in Pi's `~/.pi/agent/mcp.json`, and applies later changes to either file without a restart. A missing file counts as no servers. When both files define a server with the same name, the Pi entry is shared.

### 4. Connect your agent

On the machine where the agent runs, add the following entry to `mcpServers` in `~/.claude.json`:

```json
"tailnet": {
  "type": "stdio",
  "command": "tailnet-connector",
  "args": ["agent"],
  "env": { "TAILNET_URL": "http://100.x.y.z:8790" }
}
```

For Pi, add the same entry to `mcpServers` in `~/.pi/agent/mcp.json`, or run:

```sh
pi mcp add tailnet --env TAILNET_URL=http://100.x.y.z:8790 -- tailnet-connector agent
```

The agent proxy detects Pi through the `PI_CODING_AGENT` variable that Pi sets, and then reads Pi's config instead of Claude Code's to find the servers the agent already has.

### 5. Verify the setup

Start Claude Code or Pi and ask the agent to call the `list_machines` tool. The result lists each machine on the network along with the services it shares.

## Behavior

**Duplicate tools.** When an agent already has a server configured locally, the hub omits its own copy of that server from the agent's tool list.

**The agent proxy entry.** The connector recognizes the `tailnet` entry from step 4 and does not share it.

**Pi entries.** The connector follows Pi's rules for its entries: `type` is optional, `streamable-http` means `http`, a leading `~/` in `command`, `args` or `cwd` names the home directory, and servers with `"enabled": false` are not shared. It does not expand `${VAR}` or `!command` values, and it cannot use Pi's OAuth sign-ins, so a server that needs them fails to list its tools and is skipped.

**Server lifecycle.** Each shared server starts on its first call and stops after five minutes without calls.

**Conditional availability.** A server can be shared only while a service it depends on is reachable. See [Availability checks](#availability-checks).

## Additional configuration

### Availability checks

To share a server only while a dependency accepts connections, create `~/.config/tailnet/checks.json`:

```json
{
  "vphone": { "tcp": "vm.local:8080" },
  "my-app": { "unixSocket": "~/.my-app/sock" }
}
```

- Each key must match a server name in `mcpServers`.
- A check passes when the connection is accepted within one second. Checks run every ten seconds.
- A server without a check is always shared.

### Running as a background service

The binary can install the connector or the hub as a background service of the user who runs the command: a systemd user service on Linux, a LaunchAgent on macOS. On Windows, only the connector installs, as a scheduled task. The service starts at login and restarts after it exits, a minute later on Linux and Windows.

The service runs the binary from where it is when you install it, so first place the binary somewhere permanent.

```sh
tailnet-connector install --url http://100.x.y.z:8790   # the connector
tailnet-connector install hub                           # the hub
```

The service keeps the `PATH` of the shell that ran `install`, and the connector also gets `TAILNET_URL` from `--url`. Running `install` again replaces the service.

On Linux, read the logs with `journalctl --user -u tailnet-connector` or `journalctl --user -u tailnet-connector-hub`. On macOS, the logs are in `~/Library/Logs/tailnet-connector/connector.log` and `~/Library/Logs/tailnet-connector/hub.log`.

On Linux, a user service runs only while you are logged in, unless lingering is enabled. To keep the hub running when you are logged out, run `loginctl enable-linger`.

To remove a service, run `tailnet-connector uninstall` for the connector or `tailnet-connector uninstall hub` for the hub.

On Windows, see [Windows installation](#windows-installation).

### Windows installation

The Windows executable is cross-compiled on Linux.

1. Install the cross-compiler on Linux: `sudo apt-get install -y gcc-mingw-w64-x86-64`
2. Build the executable: `cargo build --release --target x86_64-pc-windows-gnu`
3. Copy `target/x86_64-pc-windows-gnu/release/tailnet-connector.exe` to a permanent folder on the Windows machine, since the task runs it from there.
4. From that folder, run: `start /wait tailnet-connector install --url http://100.x.y.z:8790` in cmd, or `Start-Process -Wait .\tailnet-connector -ArgumentList 'install','--url','http://100.x.y.z:8790'` in PowerShell.

The executable has no console of its own, so without `start /wait` or `Start-Process -Wait` the prompt returns before the output appears.

`install` registers a scheduled task that starts the connector in the background at each sign-in and restarts it a minute after a crash. The task runs with your user environment, so `install` also sets the user environment variables `TAILNET_URL` and `TAILNET_LOG`. The connector writes its log to `%LOCALAPPDATA%\tailnet-connector\connector.log`.

To uninstall, run `start /wait tailnet-connector uninstall`, which removes the task and the two variables.

## Environment variables

All configuration is read from environment variables. `TAILNET_URL` is the only required value.

### Connector and agent proxy

| Variable                | Default                                              | Description                                                                         |
| ----------------------- | ---------------------------------------------------- | ----------------------------------------------------------------------------------- |
| `TAILNET_URL`           | None                                                 | The hub URL. Only `http://` and `ws://` are accepted, because TLS is not supported. |
| `TAILNET_MACHINE`       | The hostname in lowercase, without a `.local` suffix | The machine name used in tool names.                                                |
| `TAILNET_IDLE_SECONDS`  | `300`                                                | Seconds without calls before a server is stopped.                                   |
| `CLAUDE_CONFIG`         | `~/.claude.json`                                     | The Claude Code file from which `mcpServers` is read.                               |
| `PI_CODING_AGENT_DIR`   | `~/.pi/agent`                                        | Pi's agent directory, whose `mcp.json` is read.                                     |
| `TAILNET_CHECKS`        | `~/.config/tailnet/checks.json`                      | The file from which availability checks are read.                                   |
| `TAILNET_CHECK_SECONDS` | `10`                                                 | The interval between availability checks.                                           |
| `TAILNET_LOG`           | None                                                 | Windows only. The log file path, since the executable has no console.               |

### Hub

| Variable                     | Default                         | Description                                                                    |
| ---------------------------- | ------------------------------- | ------------------------------------------------------------------------------ |
| `TAILNET_MCP_HOST`           | The output of `tailscale ip -4` | The address the hub listens on.                                                |
| `TAILNET_MCP_PORT`           | `8790`                          | The port the hub listens on.                                                   |
| `TAILNET_MCP_NOTIFY_SECONDS` | `300`                           | The interval at which agents are notified of tool changes. The minimum is `5`. |

## Development

Before pushing, run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo test
```

The Windows lint step requires the cross-compiler described in [Windows installation](#windows-installation).

| Path                                   | Contents                                                                    |
| -------------------------------------- | --------------------------------------------------------------------------- |
| `src/main.rs`                          | Selects the hub, connector, or agent role                                   |
| `src/connector.rs`, `src/services.rs`  | The connector: registration and server lifecycle                            |
| `src/hub_server/`                      | The hub: `/connect` for connectors, `/mcp` for agents, and the tool catalog |
| `src/agent.rs`, `src/known_servers.rs` | The agent proxy and duplicate filtering                                     |
| `src/pi_config.rs`                     | Reading Pi's `mcp.json` files                                               |
| `src/checks.rs`                        | Availability checks                                                         |
| `src/install.rs`, `src/install/`       | `install` and `uninstall`: the background service or the scheduled task     |
| `examples/echo-server.rs`              | A minimal MCP server used by the test suite                                 |

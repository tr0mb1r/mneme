# Using mneme with Hermes Agent

[Hermes Agent](https://hermes-agent.nousresearch.com/) (Nous Research)
is an MCP client, so it can use mneme as its long-term memory. This guide
covers the four ways to run them:

| Hermes | mneme | Transport | Section |
|---|---|---|---|
| bare metal | bare metal | stdio → `mneme client` → shared daemon | [1](#1-both-on-bare-metal) |
| container | container (same Compose network) | Streamable HTTP | [2](#2-both-in-containers-docker-compose) |
| container | bare metal | Streamable HTTP | [3](#3-hermes-in-a-container-mneme-on-the-host) |
| bare metal | container | Streamable HTTP | [4](#4-hermes-on-the-host-mneme-in-a-container) |

In every case `mneme init hermes` does the Hermes-side wiring:

- adds `mcp_servers.mneme` to `$HERMES_HOME/config.yaml`, editing only
  those lines, so the comments Hermes seeded into the file survive;
- installs a `mneme` skill (`$HERMES_HOME/skills/mneme/SKILL.md`) with the
  memory protocol, written against Hermes' tool names (`mcp_mneme_recall`,
  `mcp_mneme_remember`, …);
- adds a short block to `$HERMES_HOME/SOUL.md`, the one global file Hermes
  loads in every session, telling the agent to read pinned rules and
  recent context at the start of a conversation. It only does this when
  `SOUL.md` already exists, because Hermes seeds its default persona into
  a missing `SOUL.md` on first start and creating the file first would
  suppress that. Start Hermes once, then run the installer (or re-run it
  with `--upgrade`).

`mneme init hermes --show` prints the plan without writing anything, and
`mneme init hermes --uninstall` reverses it exactly.

`$HERMES_HOME` resolves from `--hermes-home`, then the `HERMES_HOME`
environment variable, then `~/.hermes`.

---

## 1. Both on bare metal

```sh
mneme init                 # once: scaffold ~/.mneme and config.toml
mneme init hermes
```

This writes:

```yaml
mcp_servers:
  # managed by mneme — undo with: mneme init hermes --uninstall
  mneme:
    command: "mneme"
    args: ["client"]
    connect_timeout: 120
```

Hermes spawns `mneme client`, which starts `mneme daemon` if needed and
shares it with every other agent on the machine (Claude Code, Cursor, …).
Nothing listens on the network.

Two things to check:

- **`mneme` must be on the `PATH` Hermes runs with.** A Hermes gateway
  running as a systemd service often doesn't see `~/.local/bin`. Either
  extend the unit's `PATH` or edit `command:` to the absolute path.
- **The first connect can be slow**, because the daemon loads the
  embedding model (and downloads it on the very first boot). Start it
  ahead of time with `mneme daemon`, or pick the small model
  (`model = "minilm-l6"` in `~/.mneme/config.toml`).

Then run `/reload-mcp` in Hermes, or restart the gateway.

---

## 2. Both in containers (Docker Compose)

mneme runs as its own service with `mneme serve`, speaking MCP over
Streamable HTTP on port 7878 of the private Compose network. Hermes
connects to `http://mneme:7878/mcp` with a bearer token. A ready-made
stack lives in [`deploy/compose/hermes/`](../deploy/compose/hermes/).

```sh
cd deploy/compose/hermes
cp .env.example .env
# set MNEME_HTTP_TOKEN, e.g. to the output of: openssl rand -base64 32
$EDITOR .env

docker compose up -d --wait mneme      # builds the image; first boot downloads the model
docker compose up -d hermes            # Hermes seeds its config.yaml and SOUL.md
docker compose run --rm hermes-setup   # mneme init hermes against Hermes' volume
docker compose restart hermes
```

`hermes-setup` is a one-shot container from the mneme image that runs:

```sh
mneme init hermes --hermes-home /opt/data --url http://mneme:7878/mcp --upgrade
```

as Hermes' UID, against the directory mounted as Hermes' `/opt/data`. The
result in Hermes' `config.yaml`:

```yaml
mcp_servers:
  # managed by mneme — undo with: mneme init hermes --uninstall
  mneme:
    url: "http://mneme:7878/mcp"
    headers:
      Authorization: "Bearer ${MNEME_HTTP_TOKEN}"
    connect_timeout: 120
```

The config only *references* the token. Hermes expands `${MNEME_HTTP_TOKEN}`
at connect time from its environment (the Compose file passes it through)
or from `$HERMES_HOME/.env`. If the variable is unset, Hermes fails the
connection and names the variable, rather than sending a literal
`${...}`.

To undo the wiring: `docker compose run --rm hermes-setup --uninstall`.

### Using the image on its own

```sh
docker build -t mneme .
docker run -d --name mneme \
  -e MNEME_HTTP_TOKEN="$(openssl rand -base64 32)" \
  -v mneme-data:/data \
  -p 127.0.0.1:7878:7878 \
  mneme
```

The image runs `mneme serve` as an unprivileged user, keeps all state
(memories, model cache, logs) in the `/data` volume, and has a health
check on `GET /healthz`. To change settings, write `/data/config.toml`
(`docker run --rm -v mneme-data:/data mneme init` creates a commented
one) or use the environment overrides below.

---

## 3. Hermes in a container, mneme on the host

Run mneme as a daemon with the HTTP listener on. Your bare-metal agents
keep using the Unix socket, and Hermes uses HTTP, both against the same
store:

```toml
# ~/.mneme/config.toml
[http]
enabled = true
# Listen where the container can reach the host. 0.0.0.0 is simplest;
# firewall port 7878 from everything except the Docker bridge, or bind
# to the bridge address (often 172.17.0.1) instead.
bind = "0.0.0.0:7878"
```

```sh
mneme stop; mneme daemon      # restart so the [http] change applies
mneme auth show-path          # the token file Hermes needs to match
```

With `[http] enabled`, the daemon's idle timeout is turned off: HTTP
clients aren't visible to the socket's connection count, so it would
otherwise shut down under an active Hermes.

By default the HTTP token is the daemon's own `~/.mneme/run/auth.token`.
To use a separate secret, set `MNEME_HTTP_TOKEN` in the daemon's
environment or point `[http] token_file` at a file.

Wire Hermes (from the host, against the directory you mount as
`/opt/data`):

```sh
mneme init hermes --hermes-home ~/.hermes --url http://host.docker.internal:7878/mcp
echo "MNEME_HTTP_TOKEN=$(cat ~/.mneme/run/auth.token)" >> ~/.hermes/.env
```

On Linux, `host.docker.internal` needs
`extra_hosts: ["host.docker.internal:host-gateway"]` on the Hermes
service. Hermes' own Compose file uses `network_mode: host`, in which case
`http://127.0.0.1:7878/mcp` works and `bind` can stay on loopback.

---

## 4. Hermes on the host, mneme in a container

Publish mneme on loopback only and point Hermes at it:

```sh
docker run -d --name mneme -e MNEME_HTTP_TOKEN=... -v mneme-data:/data \
  -p 127.0.0.1:7878:7878 mneme
mneme init hermes --url http://127.0.0.1:7878/mcp   # needs the mneme binary on the host
echo "MNEME_HTTP_TOKEN=..." >> ~/.hermes/.env
```

No mneme binary on the host? Run the installer from the image instead:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v ~/.hermes:/hermes mneme \
  init hermes --hermes-home /hermes --url http://127.0.0.1:7878/mcp
```

---

## The HTTP transport

`mneme serve` (and `mneme daemon` with `[http] enabled`) implement MCP
Streamable HTTP, protocol revision `2025-06-18`:

| Endpoint | Behaviour |
|---|---|
| `POST /mcp` | One JSON-RPC message per request. Requests return `200 application/json`; notifications return `202`. `initialize` returns an `Mcp-Session-Id` header that later requests must send. |
| `DELETE /mcp` | Ends the session named by `Mcp-Session-Id`. |
| `GET /mcp` | `405`. mneme never pushes server-to-client messages. |
| `GET /healthz` | `200 ok`, no auth. For container health checks. |

Security model:

- **Bearer token on every `/mcp` request**, compared in constant time.
  The token comes from, in order: `MNEME_HTTP_TOKEN`,
  the file named by `MNEME_HTTP_TOKEN_FILE` (Docker / Compose secrets),
  `[http] token_file`, or `~/.mneme/run/auth.token`. File tokens are
  re-read on every request, so `mneme auth rotate` takes effect without a
  restart.
- **Browser origins are refused.** A request with an `Origin` header is
  rejected unless the origin is listed in `[http] allowed_origins`
  (DNS-rebinding protection). Agents don't send `Origin`.
- **Plain HTTP.** There's no TLS in mneme. Keep the port on a private
  network (a Compose network, loopback, a firewalled bridge). To cross
  an untrusted network, put a TLS-terminating reverse proxy in front.
- **Session isolation.** Each session has its own scope cell, so one
  agent's `switch_scope` never leaks into another's. Idle sessions are
  dropped after `[http] session_idle_minutes` (default 24 h); clients
  re-initialize transparently. At most `[http] max_sessions` (default
  256) are open at once.

Settings, with their environment overrides:

| `[http]` key | Env override | Default |
|---|---|---|
| `enabled` (daemon only) | — | `false` |
| `bind` | `MNEME_HTTP_BIND` (or `mneme serve --bind`) | `127.0.0.1:7878` (`0.0.0.0:7878` in the image) |
| `token_file` | `MNEME_HTTP_TOKEN` / `MNEME_HTTP_TOKEN_FILE` | `~/.mneme/run/auth.token` |
| `allowed_origins` | `MNEME_HTTP_ALLOWED_ORIGINS` (comma-separated) | none |
| `max_sessions` | — | `256` |
| `session_idle_minutes` | — | `1440` |

`bind` takes an IP address and port. A hostname won't parse: inside a
container use `0.0.0.0:7878`.

---

## Encryption at rest in a container

Containers have no OS keyring. If you enable encryption (`mneme encrypt`),
pass the recovery phrase as `MNEME_RECOVERY_PHRASE` to the mneme
container, ideally from a Compose secret, so the store unlocks at boot.
See [Encryption at rest](../book/src/encryption.md).

---

## Troubleshooting

- **The `mcp_mneme_*` tools don't appear.** Run `hermes mcp list` to see
  the server's status, then `/reload-mcp`. In container setups,
  `docker compose logs mneme` shows rejected requests (`401` means a
  token mismatch, `403` a disallowed `Origin`).
- **`MCP server 'mneme': ${MNEME_HTTP_TOKEN} … is not set`.** Hermes
  can't see the variable. Add it to Hermes' environment or
  `$HERMES_HOME/.env`.
- **`mneme init hermes` refuses to edit `config.yaml`.** The file uses a
  layout the line editor won't guess at (for example
  `mcp_servers: {github: {...}}` in flow style, or tab indentation). The
  error includes the snippet to paste by hand.
- **The agent doesn't use memory unprompted.** Check that the block is in
  `SOUL.md` (run `mneme init hermes --upgrade` after Hermes has created
  the file) and that the `mneme` skill is listed by `/skills`.
- **mneme and Hermes' built-in memory.** They're complementary. Hermes'
  `memory` tool keeps a few kilobytes that are always in the prompt;
  mneme holds everything that needs search, history, or more room. The
  skill tells the agent which to use when.

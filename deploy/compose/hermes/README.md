# mneme + Hermes Agent (Docker Compose)

Runs mneme (`mneme serve`, MCP over Streamable HTTP) and Hermes Agent as
two services on one private network. Hermes reaches mneme at
`http://mneme:7878/mcp`; nothing is published on the host.

```sh
cp .env.example .env                   # set MNEME_HTTP_TOKEN
docker compose up -d --wait mneme      # build + first boot (downloads the model)
docker compose up -d hermes            # Hermes seeds config.yaml and SOUL.md
docker compose run --rm hermes-setup   # mneme init hermes --url http://mneme:7878/mcp
docker compose restart hermes
```

Undo the Hermes-side wiring with
`docker compose run --rm hermes-setup --uninstall`.

The full guide, including bare-metal and mixed setups, is
[`docs/HERMES_SETUP.md`](../../../docs/HERMES_SETUP.md).

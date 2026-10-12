# v1.5 release notes

> **Status: v1.5.0 shipped 2026-10-12.**
> Previous: v1.4.1 (2026-10-05).

The v1.5 cycle's theme is **hardening from day-to-day use as an agent's
long-term memory**, mostly behind Hermes Agent: pinned rules reach the
agent whole, the timeline stops collecting empty and duplicate writes,
credentials are refused at the door, and `stats` explains what
consolidation is waiting for. For what mneme adds on top of Hermes'
built-in memory, see [Hermes Agent with mneme](./hermes-with-mneme.md).

This is a **minor** bump per [Versioning](./versioning.md): two new
config keys and several tool behaviours that are now stricter, but no
new tool, and the on-disk format is unchanged. `schema_version` stays 1.
No migration. The full list is in the
[changelog](https://github.com/tr0mb1r/mneme/blob/main/CHANGELOG.md).

> **No Windows binary for 1.5.0.** The Windows release build hung, so
> the GitHub Release carries macOS and Linux archives only. Windows
> users can build with `cargo install mneme-mcp`.

---

## Stricter by default — check these before upgrading

- **Every tool rejects undeclared arguments** with `INVALID_PARAMS`
  naming the stray keys, instead of silently dropping them. The usual
  casualty is `kind` on `remember` / `update`; the argument is `type`.
- **`record_event` needs a non-empty `payload`.** Content passed at the
  top level used to be stored as `null`.
- **Credential-shaped writes are refused** (`[guardrails] secrets`,
  default `reject`; see below).
- **`mneme://context` leaves out auto-logged kinds** (`tool_call`,
  `tool_call_failed`, `session_start`, `session_end`) unless asked for.
- **Read-only tool calls are no longer logged** as `tool_call` events
  (`[episodic] auto_log_tool_calls`, default `writes`).

## New

**`[guardrails] secrets = "reject" | "warn" | "off"`.** `remember`,
`update`, `pin` and `record_event` scan for credentials in
high-confidence formats: AWS access key ids, PEM private keys, JWTs,
GitHub, Slack, OpenAI/Anthropic, Google and Stripe keys, age secret
keys. `reject` returns `secret_detected` with the pattern name and an
offset, never the matched text; `warn` stores with an advisory; `off`
skips the scan. A pinned "never store secrets" rule is now enforced
rather than advisory.

**`[episodic] auto_log_tool_calls = "writes" | "all" | "off"`.** Which
tool calls the server mirrors into the timeline. `writes` keeps
deletions and other writes auditable without flooding the timeline with
reads; `all` restores the 1.4 behaviour.

**Consolidation status in `stats`.** `stats` and `mneme://stats` report
the thresholds, the oldest hot event's age, how many events are eligible
for promotion and the days until the first one, plus the server
version. With the default 28-day hot tier nothing is promoted in the
first four weeks; the output now says so instead of looking broken.

**Exact duplicates are short-circuited.** `remember` with content
identical to a current memory in the same scope writes nothing and
returns the existing id, so retries are safe.

**`mneme://context?kinds=…&exclude_kinds=…`** to pick which event kinds
come back.

**HTTP:** `GET /health` (alias of `/healthz`) and an authenticated
`GET /version`.

## Hermes Agent

- Pinned rules are injected **whole**. The 1.4.1 hook cut every bullet
  at 300 characters, so long rules lost their endings. When the block is
  over its size limit, events and memories are shed first; pins are
  never cut.
- Event payloads are decoded (no more raw JSON), empty events are
  skipped, and memories and events are wrapped as stored data, not
  instructions.
- After Hermes compresses a conversation, the hook records Hermes' own
  summary as a `summary` event, so long sessions leave a trace even when
  the agent writes nothing.
- The installed `mneme` skill is 1.1.0 and names `keyword` search and
  `supersedes`, which agents otherwise don't discover: Hermes loads MCP
  tools on demand and learns features from the skill text.

## Upgrading

- Binary: `brew upgrade mneme` or `cargo install mneme-mcp`. Restart
  running servers.
- Hermes installs: re-run `mneme init hermes --upgrade` (with the same
  `--hermes-home` / `--url` as the first install). The hook and skill
  are copies; upgrading the binary does not update them.
- Clients that pass extra arguments will now get `INVALID_PARAMS`; fix
  the call rather than the server.

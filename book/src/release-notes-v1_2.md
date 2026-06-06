# v1.2 release notes

> **Status: v1.2.0 shipped 2026-06-06.** Previous: v1.1.1 (2026-05-23).
> This page covers everything in the v1.2 train.

The v1.2 cycle's theme is **data you can trust at rest**: every byte
mneme writes to disk can now be sealed with authenticated encryption,
opt-in, without touching the schema or breaking any existing install.
Three security hardening fixes land alongside it.

---

## Headline changes

### Encryption at rest (ADR-0013)

**Before (v1.1):** mneme's data directory — redb databases, the
procedural JSONL, session snapshots, HNSW snapshots, cold archive
quarters — sat on disk in plaintext. Filesystem permissions (`0700`
on `~/.mneme/`) were the only barrier.

**After (v1.2):**

```sh
mneme encrypt
```

That's all it takes to seal the directory. Four new subcommands
cover the full key lifecycle:

```sh
mneme encrypt             # generate keystore + migrate data in place
mneme recover --mnemonic "<12 words>"   # restore KEK on a new machine
mneme rekey               # rotate the recovery phrase (O(1), no data rewrite)
mneme decrypt --yes-i-really-mean-it    # remove keystore, turn encryption off
```

**Cipher:** XChaCha20-Poly1305 AEAD over every disk surface — semantic
WAL + redb, procedural `pinned.jsonl`, cold archive, HNSW snapshot,
and session snapshots.

**Key custody:** `~/.mneme/keystore.json` holds the wrapped DEK. The
KEK lives in the OS keyring. A BIP39 12-word recovery phrase (printed
once during `mneme encrypt`, with a 3-of-12 verification challenge)
enables re-deriving the KEK on a new or wiped machine. Headless
installs (CI, servers without a keyring backend) set
`MNEME_RECOVERY_PHRASE` instead.

**Opt-in, gated by keystore presence.** Encryption is NOT a config
flag — the on/off state is whether `~/.mneme/keystore.json` exists.
Existing v1.0/v1.1 data directories keep working without change.

**Fail-closed.** The daemon refuses to bind when a keystore is present
but no KEK can be loaded from the OS keyring or
`MNEME_RECOVERY_PHRASE`. Decrypt failures are hard errors — mneme
never silently skips a sealed record and returns plaintext instead.

**In-place migration with no plaintext residue.** `mneme encrypt`
rebuilds the redb file from an empty file (rather than running
`compact()`, which leaves freed pages intact). The live encrypted
database never contains a plaintext page. See the Security section
below for the full story.

**`--force-reinit` and `--no-verify`.** `--force-reinit` replaces an
existing keystore; without the current phrase in hand, records
encrypted under the old KEK become unrecoverable — take a `mneme
backup` first. `--no-verify` skips the 3-of-12 challenge and marks
`mnemonic_verified = false` in the keystore; useful for scripted
installs, not recommended for personal use.

See [Encryption at rest](./encryption.md) for the full setup guide,
key-custody model, and headless configuration.

### `recall_recent` gains time-range bounds

`recall_recent` now accepts `since` and `until` parameters. Both
accept an RFC3339 timestamp or a 26-character ULID (the ULID's
embedded timestamp drives the comparison). The interval is
half-open: `[since, until)` on `created_at`. When either bound is
set, the per-call result limit rises from 200 to 1,000.

No new MCP tool — this is a parameter addition to the existing
`recall_recent` tool. The call signature for callers that omit
`since`/`until` is unchanged.

---

## Security

### SEC-001 — per-connection scope isolation

**Before:** `ScopeState` was process-global inside the daemon accept
loop. A `switch_scope("X")` call from one MCP client would silently
clobber the default scope for every other concurrently-connected
client. Two agents sharing one daemon could corrupt each other's
scope routing on writes that omit an explicit `scope` argument.

**After:** each accepted connection gets its own `ScopeState`
instance. `switch_scope` is now connection-local — one agent's
scope change is invisible to all others.

### SEC-002 — size-tier gate on `record_event` payloads

`record_event` now enforces the same size tiers already present on
`remember` and `update`. Payloads above the ceiling are rejected
before reaching L3; oversized tool-call payloads no longer bypass
the guardrails that were meant to cap L4 writes.

### SEC-003 — `0o700` on data dir and subdirectories

`~/.mneme/` and every subdirectory created during scaffold are now
set to `0o700` (owner-only). The permission is re-tightened on
every daemon boot, not just on first init.

### Encryption remanence fix

`mneme encrypt` previously called redb's `compact()` to prepare
the database for re-encryption. `compact()` does not zero freed
pages — pre-encryption plaintext survived in the on-disk file's
slack space until those pages were reused. The migration now
rebuilds the redb file from an empty file, so the encrypted
database never contains a plaintext page. Caught during pre-release
testing; regression test added.

---

## Fixes

### `mneme restore` no longer false-positives on runtime dirs

`mneme restore` refused to overwrite a directory it considered
"already populated" — but it was counting the binary's own `logs/`
and daemon `run/` directories as data. A fresh machine with
`~/.mneme/logs/` present (but no memories) would get a refusal
instead of a clean restore. Fixed: runtime directories are excluded
from the clobber check.

### Daemon warns on missing `config.toml`

The daemon previously booted silently on built-in defaults when
`~/.mneme/config.toml` was absent. It now emits a `WARN`-level log
line so the absence is visible rather than invisible. Behaviour is
unchanged — defaults are still applied — but the user knows they're
running unconfigured.

---

## Config

### `[daemon] log_level` is now honored

The `log_level` field in the `[daemon]` section of `config.toml`
now controls the daemon's file-log verbosity independently of the
verbosity of short-lived CLI subcommands (which still default to
`WARN+` on stderr). Previously the setting was accepted but
ignored.

### Reserved / not-yet-wired settings documented as such

Three config keys are now explicitly marked as reserved in
`book/src/configuration.md`: `storage.encryption` (encryption is
keystore-gated, not this flag), `mcp.sse_port` (SSE transport is
not yet implemented), and `[telemetry]`. If your `config.toml`
contains any of these, they are parsed and ignored — no error —
but do not treat them as live knobs.

---

## Internal / CI

- **GitHub Actions bumped to Node 24.** `actions/checkout`,
  `actions/upload-artifact`, `actions/download-artifact`, and
  `actions/cache` all move to their Node 24 runtime versions ahead
  of GitHub's Node 20 removal.
- **Rollback promise retired.** The v1.1 → v1.0 rollback guarantee
  (and its planned CI gate) is removed. Daemon mode is now the
  sole supported path; downgrading to the v1.0 stdio binary is no
  longer a supported scenario.

---

## Upgrading from v1.0 / v1.1

No action required. `schema_version` is 1 in v1.2 (unchanged from
v1.0 and v1.1); the v1.2 binary reads an existing `~/.mneme/`
data directory as-is.

```sh
brew upgrade mneme
mneme --version   # confirm v1.2.0
```

Encryption is off by default. To enable it:

```sh
mneme stop        # if a daemon is running
mneme encrypt     # generates keystore, migrates data, prints recovery phrase
```

Store the 12-word recovery phrase somewhere safe before running
`mneme daemon` again. You cannot retrieve it after the session
ends.

---

## What's NOT in v1.2

- **SSE event-stream framing** — still deferred; `mcp.sse_port` is
  reserved but unimplemented.
- **Hybrid search (BM25 + dense)** — Tier-2 feature, no timeline.
- **cline / codex / gemini-cli installers** — still return
  `NotYetImplemented`; manual MCP-config edits still work.
- **Windows named-pipe daemon support** — code present, not yet CI-
  tested on a Windows matrix.

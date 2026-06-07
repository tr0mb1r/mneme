# v1.2 release notes

> **Status: v1.2.2 shipped 2026-06-07** (restore hardening + secret
> hygiene — see below). v1.2.1 shipped 2026-06-06 (same-day hotfix
> for the v1.2.0 encryption migration). v1.2.0 shipped 2026-06-06.
> Previous: v1.1.1 (2026-05-23).
> This page covers everything in the v1.2 train.

The v1.2 cycle's theme is **data you can trust at rest**: every byte
mneme writes to disk can now be sealed with authenticated encryption,
opt-in, without touching the schema or breaking any existing install.
Three security hardening fixes land alongside it.

---

## v1.2.2 — restore hardening + secret hygiene

Three security-review remediations; no behaviour change for
well-formed backups, no schema or config impact.

**`mneme restore` symlink escape (CWE-22).** The unpack loop only
validated entry *names* for `..` and absolute paths, and
`tar::Entry::unpack` on its own does not enforce containment — so a
crafted `.tar.gz` could plant a symlink pointing outside the data dir
and then write a child file *through* it: an arbitrary file write as
the restoring user. Restore now canonicalizes each entry's nearest
existing ancestor and refuses anything that resolves outside the
restore root, and unlinks any pre-existing symlink at a destination
before unpacking so it never follows one out of root. Legitimate
backups containing symlinks still round-trip; a regression test pins
the contract. Only restore archives you made yourself regardless —
but a malicious archive can no longer write outside the data dir.

**Recovery-phrase hygiene.** The `MNEME_RECOVERY_PHRASE` heap copy is
wiped (`zeroize`) after KEK derivation on every exit path. The process
environment itself is outside mneme's control — load the phrase from a
secrets manager, not a shell rcfile.

**Secret temp-file perms.** `keystore.json.tmp` and `auth.token.tmp`
are now created at mode `0600` rather than written at the umask
default and chmod-ed after — closing the brief window where another
local user could have opened them.

---

## v1.2.1 — encryption migration hotfix

**If you ran `mneme encrypt` on v1.2.0 and your daemon stopped
starting** (MCP clients report `-32000`): upgrade to v1.2.1 and run
`mneme encrypt` again. The re-run is a repair pass — it keeps your
existing keys and recovery phrase, re-seals the broken files, and
your data comes back intact. Nothing was lost.

```sh
brew upgrade mneme
mneme stop          # if a daemon is somehow still running
mneme encrypt       # repair pass under the existing keystore
mneme daemon        # or let your MCP host start it
```

If you already rolled back with `mneme decrypt
--yes-i-really-mean-it`, your data dir is plaintext and healthy —
upgrade and run `mneme encrypt` whenever you want encryption back
(this generates a new recovery phrase, since decrypt deleted the old
keystore).

### What was broken

The v1.2.0 `mneme encrypt` migration sealed three surfaces in a way
the runtime could not read back, and the failure killed every
subsequent daemon boot:

- **HNSW snapshot AAD mismatch.** The migration sealed
  `semantic/hnsw.idx` under the bare-filename AAD position; the boot
  loader opens with the versioned `hnsw.idx/v1` position. The tag
  check failed and the boot fell back to a cold start — recoverable
  by itself, but then:
- **The semantic WAL was never migrated.** Plaintext
  `semantic/wal/*.log` segments survived the migration, and the
  encrypted boot's WAL replay died on them. This is the bug that
  actually killed the daemon. The migration now folds outstanding
  WAL records into the snapshot and drops the segments, in both
  directions.
- **Session snapshot AAD mismatch.** Sessions were sealed under the
  full `<id>.snapshot` filename; the runtime opens with the bare
  session id. Every migrated session failed to restore.

Two more latent bugs are fixed in the same patch:

- **Re-embed on an encrypted dir wrote a plaintext snapshot.** The
  embedder-change migration (model swap) saved the rebuilt HNSW
  snapshot through the plaintext writer even when the data dir was
  encrypted — leaking every vector to disk and silently cold-starting
  the index on the next boot. The AEAD is now threaded through.
- **An encrypt re-run silently dropped every episodic record.** The
  re-run drained the WAL with a plaintext replay (dying on sealed
  frames), and rows already carrying the envelope magic were skipped
  *after* the rebuild-from-empty had deleted the database file.
  Sealed rows are now written through the raw backend byte-for-byte.

### Repair semantics

`mneme encrypt` on an already-initialised dir is no longer an error.
It loads the existing DEK (OS keyring or `MNEME_RECOVERY_PHRASE`) and
re-runs the full migration as an idempotent repair: files sealed
under the legacy v1.2.0 AAD positions are detected and re-sealed,
plaintext semantic-WAL segments are folded and dropped, and rows
already in the target format are preserved untouched.
`--force-reinit` keeps its old meaning — fresh keys, destructive
without the old phrase.

### Why the tests missed it

Each surface's seal and open sides were unit-tested with the same
helper, so a mismatch between the *migration's* seal call and the
*runtime's* open call was invisible. v1.2.1 adds the missing test
class: run the real migration over a populated data dir, then open
the result with the actual boot-path loaders — plus a fixture that
reproduces the exact broken v1.2.0 on-disk layout and proves the
repair pass fixes it.

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
mneme --version   # confirm v1.2.1
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

# Encryption at rest

Mneme v1.2 can encrypt every byte it writes to `~/.mneme/` so the
data directory is opaque to anyone who acquires a copy of the disk
without also acquiring your OS keyring or recovery phrase. The feature
is **opt-in** — your existing v1.0/v1.1 plaintext data dirs keep
working unchanged.

This page covers what it protects against, how to turn it on, how to
recover if you change machines, and what's deliberately out of scope.

## What encryption protects against

| Threat | Defended? |
|---|---|
| Lost / stolen laptop (disk image without the live session) | ✅ |
| Backup tarball leaked (uploaded to S3 / Drive by mistake) | ✅ |
| Filesystem snapshot accessed by another user with disk-level access | ✅ |
| Cross-host rsync of `~/.mneme/` to a shared box | ✅ |
| Forensic recovery from deleted blocks | ✅ |
| Root on your live machine reading process memory | ❌ |
| Timing / cache / EM side channels | ❌ |
| Swap leakage (use encrypted swap or disable swap) | ❌ |
| Attacker with the disk AND the keyring AND the mnemonic | ❌ |

Encryption at rest is one layer. For lost-laptop scenarios it pairs
well with FileVault (macOS), LUKS (Linux), and BitLocker (Windows) —
mneme strongly recommends enabling those too.

## Architecture

Two layers (envelope encryption):

```text
   BIP39 12-word mnemonic            <- you write this down once,
            │                           store offline
            │ PBKDF2-HMAC-SHA512, 2048 iter
            ▼
        KEK (32 bytes)                <- stored in OS keyring
            │                           (Keychain / Secret Service /
            │ XChaCha20-Poly1305       DPAPI), unwrapped at daemon
            │ wrap                     boot
            ▼
        DEK (32 bytes)                <- in daemon RAM only,
            │                           zeroized on shutdown
            │ XChaCha20-Poly1305      <- per record, random 24-byte
            │ per record, AAD bound      nonce, AAD bound to the
            │ to the row key             surface (redb / wal / etc.)
            ▼
   Encrypted records on disk
```

The DEK never touches disk in plaintext. The KEK can be re-derived
from the recovery phrase at any time on any machine.

**Why this shape?** Rotation cost. `mneme rekey` generates a fresh
KEK + mnemonic and re-wraps the *same* DEK — it rewrites exactly one
file (`~/.mneme/keystore.json`) regardless of how much data you've
accumulated.

## Initial setup

`mneme encrypt` is a one-shot offline operation. The mneme daemon
must NOT be running.

```bash
$ mneme stop          # if a daemon is up
$ mneme encrypt
```

You'll see something like:

```text
mneme will encrypt your memory store with a key held in your OS keyring.
You will also receive a 12-word recovery phrase. Treat it like a wallet
recovery phrase: anyone with these words can decrypt your memory.

════════════════ RECOVERY PHRASE — WRITE THIS DOWN ════════════════
  1. abandon    2. ability    3. able       4. about
  5. above      6. absent     7. absorb     8. abstract
  9. absurd    10. abuse     11. access    12. accident
══════════════════════════════════════════════════════════════════

Press ENTER when you have written down all 12 words.

Verification (attempt 1 of 3):
  Word # 7: absorb
  Word #11: access
  Word # 3: able
Phrase verified.

Encryption enabled. Keystore: /Users/you/.mneme/keystore.json
Recovery phrase verified. Run `mneme run` or `mneme daemon` to start.
```

After this:

- `~/.mneme/keystore.json` exists with mode `0o600`. It contains the
  *wrapped* DEK and the keyring account id — never the DEK itself
  in plaintext.
- Your OS keyring contains an entry under service=`mneme`,
  account=`<sha256(canonical-data-dir)[:16]>` holding the KEK.
- Future writes through `mneme run` / `mneme daemon` go through the
  encrypted storage stack: the on-disk redb file and WAL segments
  no longer contain plaintext keys or values.

### Where do I store the recovery phrase?

The wallet-industry answer applies here:

- Paper, in a safe.
- A password manager (1Password, Bitwarden) under a dedicated entry.
- A hardware vault if you have one.

NOT in: a chat thread, a screenshot, a dotfile in your home directory,
a Git repo (even a private one), an email draft. The phrase IS your
key — anyone who reads it can decrypt your memory.

### Skipping the verification challenge (`--no-verify`)

For scripted installs or CI, `mneme encrypt --no-verify` skips the
3-of-12 verification challenge. The keystore is marked
`mnemonic_verified=false` so an auditor can tell whether the user
actually wrote the phrase down. Don't use this on personal installs
— most "I lost my data" support tickets in wallet ecosystems start
with "I skipped the verification."

## Recovering on a new machine

Scenarios where the OS keyring entry is gone:

- Fresh laptop (Time Machine restore or clean install).
- Cleared keychain.
- Headless server where you copied `~/.mneme/` from another box.

```bash
$ mneme recover --mnemonic "abandon ability able about above absent absorb abstract absurd abuse access accident"
Recovery phrase verified and stored in OS keyring.
Run `mneme run` or `mneme daemon` to resume.
```

The recovery path:

1. Parses + validates the 12-word phrase.
2. Derives the KEK via BIP39 PBKDF2.
3. Verifies the derived KEK actually unwraps your `keystore.json`
   (so typos surface as a clear error, not a silent re-stash of the
   wrong key).
4. Writes the KEK to your OS keyring under the keystore's recorded
   account id.

You can now `mneme run` or `mneme daemon` again.

## Rotating the recovery phrase (`mneme rekey`)

If you suspect your current phrase is compromised, generate a new
one — without rewriting any data:

```bash
$ mneme stop
$ mneme rekey
Generating new recovery phrase. The OLD phrase will stop working.
[... new 12-word phrase + verification ...]
Rekey complete. New phrase verified.
The previous recovery phrase will no longer unwrap this keystore.
```

`mneme rekey` is **O(1)** — it re-wraps the existing DEK under a new
KEK and writes one file. The 50 GB of L3 events you've accumulated
since v1.0 don't need to be touched.

## Turning encryption off (`mneme decrypt`)

```bash
$ mneme decrypt --yes-i-really-mean-it
```

This **removes the keystore and the keyring entry**. Records that
were written while encryption was on remain on disk as opaque
ciphertext — they become unrecoverable. If you need to roll back
encryption while keeping your data, take a `mneme backup` first.

## Headless deployments (no OS keyring)

Linux servers, CI runners, and some container environments don't have
a usable Secret Service / D-Bus keyring backend. The daemon falls
back to reading the recovery phrase from `MNEME_RECOVERY_PHRASE`:

```bash
export MNEME_RECOVERY_PHRASE="abandon ability able about above absent absorb abstract absurd abuse access accident"
mneme daemon
```

Best practice is to source this from a secrets manager:

- systemd `LoadCredential=` + `ExecStart=mneme daemon`.
- AWS SSM Parameter Store (`SecureString`) + an entrypoint script.
- HashiCorp Vault.
- Docker secrets.

Never put the phrase in a shell rcfile or a Dockerfile `ENV` line.

## Multi-agent connections

**Encryption changes no wire byte.** The MCP protocol, the daemon
auth handshake, the SSE framing, the auto-spawn / reconnect logic
all continue to work identically. Every supported agent (Claude
Code, Claude Desktop, Cursor, Cline, OpenCode) keeps working without
any client-side change after you `mneme encrypt`.

The only externally visible change: if the daemon can't find the
KEK at startup, it exits cleanly with a help message pointing at
`mneme recover` rather than booting in a half-functional state.

## Backups

`mneme backup` continues to produce a single `.tar.gz` of your data
dir. On an encrypted dir, the archive contains:

- `keystore.json` (the wrapped DEK, opaque without your phrase).
- All `redb`, `wal`, and other data files as ciphertext.

**The recovery phrase does NOT travel in the tarball.** You have to
type it on the destination machine. This is intentional — if both
travel together, anyone who exfiltrates the tarball has full access.

For cross-machine restore:

```bash
# Source machine:
$ mneme backup ~/mneme-backup.tar.gz

# Destination machine:
$ scp src:~/mneme-backup.tar.gz .
$ mneme restore mneme-backup.tar.gz
$ mneme recover --mnemonic "<your 12 words>"
$ mneme daemon
```

## What's NOT encrypted

Some files in `~/.mneme/` stay plaintext on purpose:

- `config.toml` — settings, not content.
- `schema_version` — version marker.
- `run/auth.token` — the daemon's connection-auth token (mode 0600;
  separate purpose from encryption).
- `logs/mneme.log` — log lines do not contain memory bodies. If you
  have a strong threat model, set `[logging] level = "warn"`.
- `models/` — downloaded model weights are public artifacts.
- redb *keys* (ULIDs, table prefixes like `mem:` / `epi:`) — required
  for range-scan correctness; identifiers, not content.

The full inventory is in
[ADR-0013](https://github.com/tr0mb1r/mneme/blob/main/proj_docs/decisions/0013-encryption-at-rest.md)
§D7.

## Threat model boundaries (what we don't promise)

- We do not defend against a root-level adversary on your live
  machine. The DEK exists in daemon RAM while mneme is running.
- We do not provide plausible deniability — the existence of a
  `keystore.json` is observable.
- We do not provide forward secrecy. An attacker who later acquires
  your old phrase can decrypt past data; rotation (`mneme rekey`)
  invalidates only the *outer* wrap, not the DEK.

For the full threat model, see ADR-0013.

## FAQ

**Q: Will encryption slow mneme down?**
A: AEAD throughput on modern laptops is several hundred MB/s, which
is orders of magnitude above mneme's I/O rate. Steady-state latency
impact is in the microseconds per call. Cold-start cost is roughly
unchanged. Formal benches in v1.2 release notes.

**Q: Can I use a passphrase instead of a 12-word mnemonic?**
A: Not yet. The BIP39 standard allows an *extra* passphrase that
the daemon mixes into key derivation (the BIP39 "salt suffix");
exposing this is on the v1.2.x backlog.

**Q: What happens if the keyring backend changes?**
A: Standard recovery: `mneme recover --mnemonic ...` re-stashes the
KEK under whatever backend is now active. The keystore on disk is
backend-agnostic.

**Q: How does this interact with `mneme backup`?**
A: Encrypted files are already opaque, so the tarball is encrypted
too. The mnemonic does NOT travel in the tarball — you carry it
separately.

**Q: Can I encrypt only specific scopes?**
A: No. Encryption is a property of the whole data directory, not of
individual scopes. Per-scope DEKs would be useful for "share my work
scope but not personal" but break the single-user assumption.
Deferred to v1.3+ at earliest.

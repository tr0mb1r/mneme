#!/usr/bin/env python3
"""mneme context hook for Hermes Agent — installed by `mneme init hermes`.

Hermes runs this as a `pre_llm_call` shell hook: once per turn, after any
context compression and before the model sees the turn. It stays silent
on ordinary turns. On the first turn of a session, and on the first turn
after Hermes compressed the conversation, it reads `mneme://context` from
mneme and hands Hermes the pinned rules, recent activity, and memories
relevant to the user's message, which Hermes appends to that turn. That's
exactly when an agent loses track: right after compression, the rules and
state it was working with are gone from the window.

Compression is detected two ways, so a change in Hermes' summary wording
doesn't silently disable it: a new `[CONTEXT COMPACTION` summary message in
the history (fingerprinted per session), or the history getting shorter
than it was on the previous turn.

It talks MCP to mneme over whichever transport Hermes uses:

  --url URL [--token-env VAR]    Streamable HTTP (container setups)
  --command CMD [--arg A ...]    stdio, e.g. `--command mneme --arg client`

Standard library only. Any failure prints `{}` (no injection) and a
one-line reason on stderr: a memory hook must never break a turn.

Wire protocol (Hermes shell hooks): JSON payload on stdin; stdout `{}` for
no-op or `{"context": "..."}` to inject.
"""

import argparse
import hashlib
import json
import os
import queue
import subprocess
import sys
import threading
import urllib.request
from pathlib import Path

COMPACTION_MARKER = "[CONTEXT COMPACTION"
PROTOCOL_VERSION = "2025-06-18"
TIMEOUT_SECS = 15


def message_text(message):
    """Text of one history message, whatever shape Hermes used."""
    if isinstance(message, str):
        return message
    if not isinstance(message, dict):
        return ""
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
            elif isinstance(part, str):
                parts.append(part)
        return "\n".join(parts)
    return ""


def compaction_fingerprint(history):
    """Hash of the newest compaction summary in the history, or ''."""
    for message in reversed(history):
        text = message_text(message)
        if COMPACTION_MARKER in text:
            return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()[:16]
    return ""


def decide(payload, state_dir):
    """Return why to inject ('session start' / 'compression') or None, and
    record this turn's state for the next call."""
    extra = payload.get("extra") or {}
    history = extra.get("conversation_history") or []
    if not isinstance(history, list):
        history = []
    session = str(payload.get("session_id") or extra.get("session_id") or "default")
    state_path = Path(state_dir) / (hashlib.sha256(session.encode()).hexdigest()[:24] + ".json")
    try:
        previous = json.loads(state_path.read_text())
    except (OSError, ValueError):
        previous = None

    fingerprint = compaction_fingerprint(history)
    state = {"fingerprint": fingerprint, "length": len(history)}
    try:
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(json.dumps(state))
    except OSError as e:
        print(f"mneme hook: cannot save state in {state_dir}: {e}", file=sys.stderr)

    if extra.get("is_first_turn") or previous is None and not history:
        return "session start"
    if previous is None:
        # First time we've seen this session but it already has history
        # (the hook was installed mid-session, or the session was
        # rotated by compression): reload once to be safe.
        return "compression" if fingerprint else "session start"
    if fingerprint and fingerprint != previous.get("fingerprint"):
        return "compression"
    if len(history) < int(previous.get("length") or 0):
        return "compression"
    return None


def token_from(var, hermes_home):
    """Bearer token from the environment, else from $HERMES_HOME/.env
    (Hermes scrubs secrets from hook environments on multiplexed
    gateways)."""
    value = os.environ.get(var, "").strip()
    if value:
        return value
    env_file = Path(hermes_home) / ".env"
    try:
        for line in env_file.read_text().splitlines():
            line = line.strip()
            if line.startswith("export "):
                line = line[len("export "):]
            key, sep, val = line.partition("=")
            if sep and key.strip() == var:
                return val.strip().strip('"').strip("'")
    except OSError:
        pass
    return ""


def initialize_request():
    return {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "mneme-hermes-hook", "version": "1"},
        },
    }


INITIALIZED = {"jsonrpc": "2.0", "method": "notifications/initialized"}


def read_request(uri):
    return {"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": uri}}


def resource_text(response):
    if "error" in response:
        raise RuntimeError(f"mneme error: {response['error'].get('message')}")
    contents = response.get("result", {}).get("contents") or []
    if not contents:
        raise RuntimeError("mneme returned no contents")
    return contents[0].get("text", "")


def read_over_http(url, token, uri):
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"

    def post(body, session=None):
        h = dict(headers)
        if session:
            h["Mcp-Session-Id"] = session
        req = urllib.request.Request(url, json.dumps(body).encode(), h, method="POST")
        with urllib.request.urlopen(req, timeout=TIMEOUT_SECS) as resp:
            data = resp.read()
            return resp.headers.get("Mcp-Session-Id"), (json.loads(data) if data else None)

    session, _ = post(initialize_request())
    post(INITIALIZED, session)
    _, response = post(read_request(uri), session)
    if session:
        try:
            h = dict(headers)
            h["Mcp-Session-Id"] = session
            urllib.request.urlopen(
                urllib.request.Request(url, None, h, method="DELETE"), timeout=TIMEOUT_SECS
            ).close()
        except Exception:
            pass
    return resource_text(response)


def read_over_stdio(argv, uri):
    proc = subprocess.Popen(
        argv,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
    )
    lines = queue.Queue()

    def pump():
        for line in proc.stdout:
            lines.put(line)
        lines.put(None)

    threading.Thread(target=pump, daemon=True).start()

    def send(message):
        proc.stdin.write(json.dumps(message) + "\n")
        proc.stdin.flush()

    def wait_for(request_id):
        while True:
            line = lines.get(timeout=TIMEOUT_SECS)
            if line is None:
                raise RuntimeError("mneme closed the connection")
            line = line.strip()
            if not line:
                continue
            message = json.loads(line)
            if message.get("id") == request_id:
                return message

    try:
        send(initialize_request())
        wait_for(1)
        send(INITIALIZED)
        send(read_request(uri))
        return resource_text(wait_for(2))
    finally:
        try:
            proc.stdin.close()
            proc.wait(timeout=5)
        except Exception:
            proc.kill()


def bullet(text, limit=300):
    one_line = " ".join(str(text).split())
    return one_line if len(one_line) <= limit else one_line[: limit - 1] + "…"


def render(context_json, reason, max_chars):
    data = json.loads(context_json)
    if reason == "compression":
        head = ("[mneme] The conversation was just compressed. Reloaded from long-term "
                "memory (mneme) so you keep your rules and current state:")
    else:
        head = "[mneme] Session start. Loaded from long-term memory (mneme):"
    out = [head]
    pinned = data.get("procedural") or []
    if pinned:
        out.append("\nPinned rules (binding, follow them):")
        out += [f"- {bullet(p.get('content', ''))}" for p in pinned]
    semantic = data.get("semantic") or []
    if semantic:
        out.append("\nMemories relevant to this message:")
        out += [f"- {bullet(m.get('content', ''))} (id {m.get('id')})" for m in semantic]
    episodic = data.get("episodic") or []
    events = [e for e in episodic if e.get("kind") not in ("tool_call", "session_start", "session_end")]
    if events:
        out.append("\nRecent events:")
        for e in events[:15]:
            when = str(e.get("created_at", ""))[:16].replace("T", " ")
            payload = e.get("payload")
            if isinstance(payload, dict):
                text = payload.get("text") or payload.get("content") or json.dumps(payload, ensure_ascii=False)
            else:
                text = payload
            out.append(f"- {when} {e.get('kind')}: {bullet(text, 200)}")
    if len(out) == 1:
        return None  # nothing stored yet; stay quiet
    out.append("\nUse mcp_mneme_recall for anything else from earlier sessions.")
    text = "\n".join(out)
    if len(text) > max_chars:
        text = text[: max_chars - 1] + "…"
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--url")
    parser.add_argument("--token-env", default="MNEME_HTTP_TOKEN")
    parser.add_argument("--command")
    parser.add_argument("--arg", action="append", default=[])
    parser.add_argument("--state-dir")
    parser.add_argument("--max-chars", type=int, default=8000)
    args = parser.parse_args()

    hermes_home = os.environ.get("HERMES_HOME") or str(Path.home() / ".hermes")
    state_dir = args.state_dir or str(Path(hermes_home) / "mneme-hook-state")
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        payload = {}
    if payload.get("hook_event_name") not in (None, "pre_llm_call"):
        print("{}")
        return

    try:
        reason = decide(payload, state_dir)
        if reason is None:
            print("{}")
            return
        user_message = message_text({"content": (payload.get("extra") or {}).get("user_message")})
        uri = "mneme://context"
        if user_message.strip():
            uri += "?q=" + urllib.request.quote(bullet(user_message, 200), safe="")
        if args.url:
            context_json = read_over_http(args.url, token_from(args.token_env, hermes_home), uri)
        elif args.command:
            context_json = read_over_stdio([args.command] + args.arg, uri)
        else:
            raise RuntimeError("pass --url or --command")
        text = render(context_json, reason, args.max_chars)
        print(json.dumps({"context": text}) if text else "{}")
    except Exception as e:  # never break the turn
        print(f"mneme hook: {type(e).__name__}: {e}", file=sys.stderr)
        print("{}")


if __name__ == "__main__":
    main()

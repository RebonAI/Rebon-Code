#!/usr/bin/env python3
"""Talk ACP to a rebon session owner over its IPC port, as a third party would.

This is deliberately *not* rebon's own client. It is a bare socket and a line
of JSON per message, so what it proves is what a stranger on the wire can do:
the control plane is reachable without linking anything of rebon's.

Usage:

    python scripts/probe-session-acp.py --port 61201 --token <token>
    python scripts/probe-session-acp.py --port 61201 --token <token> --case ping
    python scripts/probe-session-acp.py --port 61201 --token bad --case bad-token

Every case prints what it sent, what came back, and how long the round trip
took. Exit status is the number of failed expectations, so it is usable from a
script.

The owner speaks NDJSON: one JSON-RPC object per line, no framing header. A
request carries `id`; a notification does not and is never answered.
Authentication is once per connection, in `initialize`, under
`_meta.rebon.token` — see `crates/rebon-session-runtime/src/host/ipc/acp_gate.rs`.
"""

import argparse
import json
import socket
import sys
import time

# A read that timed out, as distinct from a peer that hung up.
TIMEOUT = object()

# From `rebon_proto::error_code`.
METHOD_NOT_FOUND = -32601
INVALID_REQUEST = -32600
UNAUTHENTICATED = -32011

# From `rebon_session_host::session_ext::method`.
EXTENSION_METHODS = [
    "_session/ping",
    "_session/status",
    "_session/run_command",
    "_session/set_permission_mode",
    "_session/set_option",
    "_session/rewind",
    "_session/compact",
    "_session/reconcile_plugins",
    "_session/answer_questions",
    "_session/task_reply",
    "_session/cancel_tasks",
    "_session/lease",
    "_session/release_lease",
    "_session/subscribe",
    "_session/cancel_call",
    "_session/hello",
    "_session/turn",
    "_session/status_changed",
    "_session/gap",
]


class Conn:
    """One connection. Not reused across cases that expect it to be closed."""

    def __init__(self, port, timeout=10.0):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.sock.settimeout(timeout)
        self.buf = b""
        self.next_id = 1

    def send(self, obj):
        line = json.dumps(obj, separators=(",", ":")).encode("utf-8") + b"\n"
        self.sock.sendall(line)
        return line.decode("utf-8").rstrip("\n")

    def read_line(self):
        """One JSON object, `None` when the peer closed, `TIMEOUT` when it is
        simply quiet.

        The two are not the same, and conflating them cost a round: a stream
        with nothing to say for a second looked exactly like a stream that had
        ended.
        """
        while b"\n" not in self.buf:
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                return TIMEOUT
            if not chunk:
                return None
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        text = line.decode("utf-8", "replace").strip()
        if not text:
            return self.read_line()
        return json.loads(text)

    def call(self, method, params=None, meta=None):
        """A request. Returns (response, seconds, sent_text)."""
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        self.next_id += 1
        if params is not None:
            msg["params"] = params
        if meta is not None:
            msg.setdefault("params", {})
            msg["params"]["_meta"] = meta
        started = time.perf_counter()
        sent = self.send(msg)
        reply = self.read_line()
        return reply, time.perf_counter() - started, sent

    def notify(self, method, params=None, meta=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if meta is not None:
            msg.setdefault("params", {})
            msg["params"]["_meta"] = meta
        started = time.perf_counter()
        sent = self.send(msg)
        return time.perf_counter() - started, sent

    def respond(self, request_id, result, meta=None):
        """Answer a request the *owner* sent us.

        The permission round trip runs this way round: the owner asks, the
        client answers on the same connection with the same id. Without this
        a watcher can see a question and has no way to end it.
        """
        msg = {"jsonrpc": "2.0", "id": request_id, "result": result}
        if meta is not None:
            msg["result"]["_meta"] = meta
        return self.send(msg)

    def peer_closed(self, wait=2.0):
        """True when the peer has hung up (or sent nothing at all)."""
        self.sock.settimeout(wait)
        try:
            return self.sock.recv(1) == b""
        except socket.timeout:
            return False
        except OSError:
            return True

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def token_meta(token):
    return {"rebon": {"token": token}}


class Report:
    def __init__(self):
        self.rows = []
        self.failures = 0

    def record(self, case, sent, got, seconds, expectation, ok):
        self.rows.append((case, sent, got, seconds, expectation, ok))
        if not ok:
            self.failures += 1
        mark = "ok  " if ok else "FAIL"
        print("[%s] %-28s %8.3f ms  %s" % (mark, case, seconds * 1000.0, expectation))
        print("       sent: %s" % (sent if sent else "-"))
        got_text = json.dumps(got, separators=(",", ":")) if got is not None else "<closed>"
        if len(got_text) > 400:
            got_text = got_text[:400] + " ...(truncated)"
        print("       recv: %s" % got_text)


def initialize(conn, token):
    return conn.call(
        "initialize",
        {"protocolVersion": 1, "clientCapabilities": {}},
        token_meta(token),
    )


def case_initialize(port, token, report):
    conn = Conn(port)
    try:
        reply, secs, sent = initialize(conn, token)
        meta = ((reply or {}).get("result") or {}).get("_meta") or {}
        advertised = (meta.get("rebon") or {}).get("methods")
        ok = advertised == EXTENSION_METHODS
        detail = "_meta.rebon.methods == session_ext::method::ALL (%d names)" % len(
            EXTENSION_METHODS
        )
        if not ok and advertised is not None:
            missing = [m for m in EXTENSION_METHODS if m not in advertised]
            extra = [m for m in advertised if m not in EXTENSION_METHODS]
            detail += "  missing=%s extra=%s" % (missing, extra)
        report.record("initialize", sent, reply, secs, detail, ok)
        return conn
    except Exception:
        conn.close()
        raise


def case_bad_token(port, report):
    conn = Conn(port)
    try:
        reply, secs, sent = initialize(conn, "not-the-token")
        code = ((reply or {}).get("error") or {}).get("code")
        closed = conn.peer_closed()
        ok = code == UNAUTHENTICATED and closed
        report.record(
            "initialize (bad token)",
            sent,
            reply,
            secs,
            "error %d and the connection closes (got code=%s closed=%s)"
            % (UNAUTHENTICATED, code, closed),
            ok,
        )
    finally:
        conn.close()


def case_before_initialize(port, report):
    conn = Conn(port)
    try:
        reply, secs, sent = conn.call("_session/ping", {})
        code = ((reply or {}).get("error") or {}).get("code")
        closed = conn.peer_closed(wait=1.0)
        ok = code == INVALID_REQUEST and not closed
        report.record(
            "ping before initialize",
            sent,
            reply,
            secs,
            "error %d and the connection survives (got code=%s closed=%s)"
            % (INVALID_REQUEST, code, closed),
            ok,
        )
    finally:
        conn.close()


def case_watch(port, token, report, seconds, since, answer_permission=None):
    """Subscribe and print everything the owner pushes for a while.

    This is the read side of the control plane: `_session/hello` first, then
    `session/update`, `_session/turn`, `_session/status_changed`,
    `_session/gap`, and `session/request_permission` when the owner has a
    question. Nothing is asserted — it is a window onto the stream, for
    watching a splice or catching a permission that is raised while it runs.
    """
    conn = Conn(port, timeout=max(seconds + 5.0, 15.0))
    try:
        reply, secs, sent = initialize(conn, token)
        report.record("initialize (for watch)", sent, reply, secs,
                      "authenticated", reply is not None and "result" in reply)

        params = {} if since is None else {"since": since}
        reply, secs, sent = conn.call("_session/subscribe", params)
        report.record("subscribe", sent, reply, secs,
                      "the owner accepted the subscription",
                      reply is not None and "result" in reply)

        print()
        print("--- streaming for %.0fs ---" % seconds)
        deadline = time.perf_counter() + seconds
        seen = 0
        while time.perf_counter() < deadline:
            conn.sock.settimeout(max(0.5, deadline - time.perf_counter()))
            try:
                msg = conn.read_line()
            except socket.timeout:
                continue
            if msg is TIMEOUT:
                continue
            if msg is None:
                print("  <the owner closed the stream>")
                break
            seen += 1
            method = msg.get("method", "(response id=%s)" % msg.get("id"))
            params = msg.get("params") or {}
            meta = (params.get("_meta") or {}).get("rebon") or {}
            # `_session/hello` carries the stamp in its own params; a
            # `session/update` has to put it in `_meta`, because the standard
            # shape has nowhere for it. Read both.
            cursor = meta.get("cursor", params.get("cursor"))
            epoch = meta.get("epoch", params.get("epoch"))
            stamp = ""
            if cursor is not None or epoch is not None:
                stamp = "  cursor=%s epoch=%s" % (cursor, epoch)
            if "queryId" in meta:
                stamp += "  queryId=%s" % meta.get("queryId")
            body = json.dumps(msg, separators=(",", ":"))
            if len(body) > 300:
                body = body[:300] + " ...(truncated)"
            print("  %-32s%s" % (method, stamp))
            print("      %s" % body)

            # The owner asks; the client answers on the same connection with
            # the same id. Without this the question sits there until the
            # 120 s deadline denies it.
            if (answer_permission
                    and msg.get("method") == "session/request_permission"
                    and msg.get("id") is not None):
                options = ((msg.get("params") or {}).get("options") or [])
                chosen = None
                for opt in options:
                    if opt.get("optionId") == answer_permission:
                        chosen = opt.get("optionId")
                        break
                if chosen is None and options:
                    chosen = options[0].get("optionId")
                sent_back = conn.respond(
                    msg["id"],
                    {"outcome": {"outcome": "selected", "optionId": chosen}},
                )
                print("      -> answered with optionId=%s" % chosen)
                print("      %s" % sent_back)
        print("--- %d message(s) ---" % seen)
        return seen
    finally:
        conn.close()


def case_detect(port, token, report):
    """Which protocol is on the other end, without assuming either.

    A worker that predates E3 answers `initialize` in the legacy envelope —
    `{"ok":false,"error":"missing field `token` ..."}` — because it is trying
    to decode the frame as a legacy request and `initialize` has no `token`
    field at the top level. That answer has no `jsonrpc` key at all, which is
    the whole discriminator: a client can tell the two apart in one round trip
    without probing method by method, and without a bad guess costing it the
    connection.
    """
    conn = Conn(port)
    try:
        reply, secs, sent = initialize(conn, token)
        reply = reply or {}
        speaks_acp = "jsonrpc" in reply
        legacy = "ok" in reply and "jsonrpc" not in reply
        verdict = "ACP" if speaks_acp else ("legacy (pre-E3)" if legacy else "unrecognised")
        report.record(
            "detect protocol",
            sent,
            reply,
            secs,
            "one round trip decides: %s" % verdict,
            speaks_acp or legacy,
        )
        return verdict
    finally:
        conn.close()


def simple_cases(conn, report):
    reply, secs, sent = conn.call("_session/ping", {})
    report.record("ping", sent, reply, secs, "a result, not an error",
                  reply is not None and "result" in reply)

    reply, secs, sent = conn.call("_session/status", {})
    report.record("status", sent, reply, secs, "a status snapshot",
                  reply is not None and "result" in reply)

    # `RunCommandParams` is `{name, args}` — not `{command}`.
    reply, secs, sent = conn.call("_session/run_command", {"name": "hooks"})
    report.record("run_command hooks", sent, reply, secs,
                  "runs the command and returns its output",
                  reply is not None and "result" in reply)

    reply, secs, sent = conn.call("no/such/method", {})
    code = ((reply or {}).get("error") or {}).get("code")
    report.record("unknown method", sent, reply, secs,
                  "error %d (got %s)" % (METHOD_NOT_FOUND, code),
                  code == METHOD_NOT_FOUND)


def case_prompt_and_cancel(conn, report, session_id):
    """The model is unreachable on purpose; what matters is how it fails.

    The session id has to be the owner's own: a `session/prompt` for any other
    id is refused as an owner fence (-32012) before it reaches the turn loop,
    which tests the fence rather than the prompt path.
    """
    reply, secs, sent = conn.call(
        "session/prompt",
        {"sessionId": session_id, "prompt": [{"type": "text", "text": "say hi"}]},
    )
    report.record("session/prompt", sent, reply, secs,
                  "answers rather than hanging (failure is expected here)",
                  reply is not None)

    secs, sent = conn.notify("session/cancel", {"sessionId": session_id})
    report.record("session/cancel (notify)", sent, None, secs,
                  "a notification is never answered", True)

    reply, secs, sent = conn.call("_session/ping", {})
    report.record("ping after cancel", sent, reply, secs,
                  "the connection is still usable",
                  reply is not None and "result" in reply)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--token", default="")
    ap.add_argument("--session", default="",
                    help="the owner's session id; required for the prompt case")
    ap.add_argument("--case", default="all")
    ap.add_argument("--seconds", type=float, default=45.0,
                    help="how long --case watch streams for")
    ap.add_argument("--answer-permission", dest="answer_permission", default=None,
                    help="answer any session/request_permission with this optionId "
                         "(or the first option offered)")
    ap.add_argument("--since", type=int, default=None,
                    help="subscribe from this cursor; omit to start at the attach point")
    ap.add_argument("--timeout", type=float, default=10.0)
    args = ap.parse_args()

    report = Report()
    case = args.case

    try:
        if case == "watch":
            case_watch(args.port, args.token, report, args.seconds, args.since,
                       args.answer_permission)
            print()
            print("%d case(s), %d failed" % (len(report.rows), report.failures))
            return report.failures
        if case == "detect":
            case_detect(args.port, args.token, report)
            print()
            print("%d case(s), %d failed" % (len(report.rows), report.failures))
            return report.failures
        if case in ("all", "bad-token"):
            case_bad_token(args.port, report)
        if case in ("all", "before-initialize"):
            case_before_initialize(args.port, report)
        if case in ("all", "initialize", "ping", "status", "run_command",
                    "unknown", "prompt"):
            conn = case_initialize(args.port, args.token, report)
            try:
                if case in ("all", "ping", "status", "run_command", "unknown"):
                    simple_cases(conn, report)
                if case in ("all", "prompt"):
                    case_prompt_and_cancel(conn, report, args.session)
            finally:
                conn.close()
    except (ConnectionRefusedError, OSError) as err:
        print("connection failed: %s" % err)
        return 90

    print()
    print("%d case(s), %d failed" % (len(report.rows), report.failures))
    return report.failures


if __name__ == "__main__":
    sys.exit(main())

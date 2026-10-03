#!/usr/bin/env python3
"""Claude Agent Graph: a live view of your Claude Code sessions and subagents.

Usage:  agent-graph [--browser] [--port N]
Opens a native window (GTK WebKit on Linux, pywebview on macOS/Windows when installed);
otherwise, or with --browser, serves on localhost and opens your default browser.
"""

import argparse
import json
import os
import secrets
import sys
import threading
import time
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from . import workflows
from .watcher import Graph

HERE = os.path.dirname(os.path.abspath(__file__))
POLL_SECONDS = 0.5
HEARTBEAT_SECONDS = 3

graph = Graph()
TOKEN = secrets.token_urlsafe(24)  # guards the API against other pages posting to localhost
changed = threading.Condition()


def notify():
    with changed:
        changed.notify_all()


workflows.wfstore.project_hint = graph.project_dirs  # projects your sessions ran in may hold workflows
runner = workflows.Runner(graph, notify)


wf_version = [0]  # bumps when a workflow YAML changes on disk


def poll_forever():
    while True:
        try:
            wf_before = wf_version[0]
            wf_version[0] = workflows.wfstore.version()
            if graph.scan(live=True) or wf_version[0] != wf_before:
                notify()
        except Exception as exc:  # keep watching even if one file is odd
            print("scan error:", exc, file=sys.stderr)
        time.sleep(POLL_SECONDS)


def kill(body):
    ok, message = graph.kill_session(str(body["id"]))
    if ok:
        notify()
    return {"ok": ok, "message": message}


def ok(value=None, **extra):
    return {"ok": True, "data": value, **extra}


API_GET = {
    "/api/permissions": lambda: ok(workflows.read_permissions()),
    "/api/workflows": lambda: ok(workflows.workflows_state()),
    "/api/ides": lambda: ok(workflows.list_ides()),
    "/api/defaults": lambda: ok(workflows.wfstore.load_defaults()),
}
API_POST = {
    "/kill": kill,
    "/api/permissions": lambda b: ok(workflows.write_permissions(b)),
    "/api/workflows/save": lambda b: ok(workflows.save_workflow(b)),
    "/api/workflows/delete": lambda b: ok(workflows.delete_workflow(b["id"])),
    "/api/workflows/open": lambda b: ok(workflows.open_workflow_file(b.get("id"), b.get("what"))),
    "/api/agents/save": lambda b: ok(workflows.wfstore.save_user_agent(b["agent"], b.get("category") or "My helpers", b.get("oldName"))),
    "/api/agents/delete": lambda b: ok(workflows.wfstore.delete_user_agent(b["name"])),
    "/api/steps/save": lambda b: ok(workflows.wfstore.save_user_step(b.get("key"), b["step"], b.get("oldKey"))),
    "/api/steps/delete": lambda b: ok(workflows.wfstore.delete_user_step(b["key"])),
    "/api/ides/set": lambda b: ok(workflows.set_ide(b.get("ide"))),
    "/api/runs/ide": lambda b: ok(runner.open_ide(b["id"], b.get("path"))),
    "/api/runs/start": lambda b: ok(runner.start(b["id"])),
    "/api/runs/stop": lambda b: ok(runner.stop(b["id"])),
    "/api/runs/replay": lambda b: ok(runner.replay(b["id"], b["step"], bool(b.get("only")))),
    "/api/runs/detail": lambda b: ok(runner.detail(b["id"])),
    "/api/runs/folder": lambda b: ok(runner.open_folder(b["id"])),
    "/api/runs/log": lambda b: ok(runner.log(b["id"], b["session"])),
    "/api/sessions/log": lambda b: ok(runner.session_log(str(b["session"]))),
    "/api/runs/review": lambda b: ok(runner.review(b["id"], b.get("decision"), b.get("feedback", ""))),
    "/api/runs/save": lambda b: ok(runner.write_artefact(b["id"], b["path"], str(b["text"]))),
    "/api/rewrite": lambda b: ok(workflows.rewrite_text(b.get("text"), b.get("kind", "step"), b.get("context", ""))),
    "/api/runs/file": lambda b: ok(runner.read_artefact(b["id"], b["path"])),
    "/api/runs/open": lambda b: ok(runner.open_artefact(b["id"], b["path"], bool(b.get("folder")))),
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def local_host(self):
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0]
        return host in ("127.0.0.1", "localhost")

    def send_json(self, data):
        out = json.dumps(data).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def call_api(self, fn, *args):
        if self.headers.get("X-Token") != TOKEN:
            return self.send_error(403)
        try:
            result = fn(*args)
        except (ValueError, KeyError, TypeError, OSError) as exc:
            result = {"ok": False, "message": str(exc) or exc.__class__.__name__}
        self.send_json(result)

    def do_GET(self):
        if not self.local_host():
            return self.send_error(403)
        if self.path in ("/", "/index.html"):
            with open(os.path.join(HERE, "index.html"), "rb") as f:
                body = f.read().replace(b"__TOKEN__", TOKEN.encode())
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == "/events":
            self.stream()
        elif self.path in API_GET:
            self.call_api(API_GET[self.path])
        else:
            self.send_error(404)

    def do_POST(self):
        if not self.local_host():
            return self.send_error(403)
        fn = API_POST.get(self.path)
        if not fn:
            return self.send_error(404)
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        except ValueError:
            return self.send_json({"ok": False, "message": "Bad JSON."})
        self.call_api(fn, body)

    def stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        seq = 0
        try:
            while True:
                snap = graph.snapshot(since_seq=seq)
                snap["runs"] = runner.summary()
                snap["wfVersion"] = wf_version[0]
                seq = snap["seq"]
                self.wfile.write(b"data: " + json.dumps(snap).encode() + b"\n\n")
                self.wfile.flush()
                with changed:
                    changed.wait(HEARTBEAT_SECONDS)
        except (BrokenPipeError, ConnectionResetError):
            pass


def serve(port):
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server.server_address[1]


def open_window(url):
    try:
        open_gtk_window(url)
    except (ImportError, ValueError):
        import webview  # pywebview: native window on macOS, Windows and Linux

        webview.create_window("Claude Agent Graph", url, width=1400, height=900)
        webview.start()


def open_gtk_window(url):
    import gi

    gi.require_version("Gtk", "3.0")
    gi.require_version("WebKit2", "4.1")
    from gi.repository import Gtk, WebKit2

    win = Gtk.Window(title="Claude Agent Graph")
    win.set_default_size(1400, 900)
    win.set_icon_name("network-workgroup")
    view = WebKit2.WebView()
    view.load_uri(url)
    win.add(view)
    win.connect("destroy", Gtk.main_quit)
    win.show_all()
    Gtk.main()


def main():
    if os.name == "nt" and not sys.flags.utf8_mode:
        # Windows defaults to a legacy code page; the YAML and transcripts are UTF-8.
        import subprocess

        sys.exit(subprocess.call([sys.executable, "-X", "utf8", "-m", "agent_graph", *sys.argv[1:]]))
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--browser", action="store_true", help="open in the default browser instead of a window")
    ap.add_argument("--port", type=int, default=0, help="port to serve on (default: random free port)")
    args = ap.parse_args()

    graph.scan(live=False)  # history: shown, but not animated
    threading.Thread(target=poll_forever, daemon=True).start()
    port = serve(args.port or (8765 if args.browser else 0))
    url = f"http://127.0.0.1:{port}/"

    if not args.browser:
        try:
            open_window(url)
            return
        except Exception as exc:  # no GUI toolkit: the browser works everywhere
            print(f"No native window available ({exc}); opening your browser instead.\n"
                  "  (pip install pywebview for a window)", file=sys.stderr)
    print("Claude Agent Graph running at", url)
    webbrowser.open(url)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()

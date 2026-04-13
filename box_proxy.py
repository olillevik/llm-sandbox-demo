#!/usr/bin/env python3
from __future__ import annotations

import argparse
import http.client
import json
import select
import socket
import sys
import threading
from datetime import datetime, timezone
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Iterable
from urllib.parse import urlsplit

HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}


def normalize_host(value: str) -> str:
    value = value.strip().lower()
    if "://" in value:
        value = urlsplit(value).hostname or ""
    else:
        value = value.split("/")[0]
        value = value.split(":")[0]
    return value.strip().strip(".")


class ProxyState:
    def __init__(self, allowed_hosts_file: Path, pending_log_file: Path, workspace: str):
        self.allowed_hosts_file = allowed_hosts_file
        self.pending_log_file = pending_log_file
        self.workspace = workspace
        self._lock = threading.Lock()

    def allowed_hosts(self) -> set[str]:
        if not self.allowed_hosts_file.exists():
            return set()
        return {
            normalize_host(line)
            for line in self.allowed_hosts_file.read_text(encoding="utf-8").splitlines()
            if normalize_host(line)
        }

    def is_allowed(self, host: str) -> bool:
        return normalize_host(host) in self.allowed_hosts()

    def log_blocked(self, host: str, port: int | None, scheme: str) -> None:
        record = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "host": normalize_host(host),
            "port": port,
            "scheme": scheme,
            "workspace": self.workspace,
        }
        with self._lock:
            with self.pending_log_file.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(record, sort_keys=True))
                handle.write("\n")


class ThreadedHTTPServer(ThreadingHTTPServer):
    daemon_threads = True


class ProxyHandler(BaseHTTPRequestHandler):
    server: "ProxyServer"
    protocol_version = "HTTP/1.1"

    def do_CONNECT(self) -> None:
        host, port = self._parse_connect_target(self.path)
        if not self.server.state.is_allowed(host):
            self.server.state.log_blocked(host, port, "connect")
            self.send_error(HTTPStatus.FORBIDDEN, f"{host} is not approved")
            return

        try:
            upstream = socket.create_connection((host, port))
        except OSError as exc:
            self.send_error(HTTPStatus.BAD_GATEWAY, str(exc))
            return

        self.send_response(HTTPStatus.OK, "Connection Established")
        self.end_headers()
        self._tunnel(self.connection, upstream)

    def do_GET(self) -> None:
        self._proxy_plain_http()

    def do_POST(self) -> None:
        self._proxy_plain_http()

    def do_PUT(self) -> None:
        self._proxy_plain_http()

    def do_PATCH(self) -> None:
        self._proxy_plain_http()

    def do_DELETE(self) -> None:
        self._proxy_plain_http()

    def do_HEAD(self) -> None:
        self._proxy_plain_http()

    def _proxy_plain_http(self) -> None:
        target = urlsplit(self.path)
        host_header = target.netloc or self.headers.get("Host", "")
        host = target.hostname or host_header
        port = target.port
        if not port and host_header and ":" in host_header:
            host, _, raw_port = host_header.rpartition(":")
            port = int(raw_port)
        port = port or 80
        if not host:
            self.send_error(HTTPStatus.BAD_REQUEST, "missing target host")
            return

        if not self.server.state.is_allowed(host):
            self.server.state.log_blocked(host, port, "http")
            self.send_error(HTTPStatus.FORBIDDEN, f"{host} is not approved")
            return

        path = target.path or "/"
        if target.query:
            path = f"{path}?{target.query}"

        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length) if length else None

        request_headers = {}
        for key, value in self.headers.items():
            if key.lower() in self._hop_by_hop_headers(self.headers) or key.lower() == "host":
                continue
            request_headers[key] = value
        request_headers["Host"] = host_header or (host if port == 80 else f"{host}:{port}")
        request_headers["Connection"] = "close"

        try:
            upstream = http.client.HTTPConnection(host, port, timeout=30)
            upstream.request(self.command, path, body=body, headers=request_headers)
            response = upstream.getresponse()
        except OSError as exc:
            self.send_error(HTTPStatus.BAD_GATEWAY, str(exc))
            return

        try:
            self.close_connection = True
            self.send_response(response.status, response.reason)
            response_hop_by_hop = self._hop_by_hop_headers(response.headers)
            for key, value in response.getheaders():
                if key.lower() in response_hop_by_hop:
                    continue
                self.send_header(key, value)
            self.send_header("Connection", "close")
            self.end_headers()

            if self.command != "HEAD":
                while True:
                    chunk = response.read(65536)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
        finally:
            response.close()
            upstream.close()

    def _hop_by_hop_headers(self, headers: http.client.HTTPMessage) -> set[str]:
        connection_tokens = {
            token.strip().lower()
            for token in headers.get("Connection", "").split(",")
            if token.strip()
        }
        return HOP_BY_HOP_HEADERS | connection_tokens

    def _parse_connect_target(self, value: str) -> tuple[str, int]:
        host, _, port = value.rpartition(":")
        if not host:
            host, port = value, "443"
        return normalize_host(host), int(port)

    def _tunnel(self, client: socket.socket, upstream: socket.socket) -> None:
        sockets = [client, upstream]
        while True:
            readable, _, exceptional = select.select(sockets, [], sockets, 1.0)
            if exceptional:
                break
            if not readable:
                continue
            for sock in readable:
                other = upstream if sock is client else client
                data = sock.recv(65536)
                if not data:
                    return
                other.sendall(data)

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write("%s - - [%s] %s\n" % (self.client_address[0], self.log_date_time_string(), fmt % args))


class ProxyServer(ThreadingHTTPServer):
    def __init__(self, server_address: tuple[str, int], handler_class: type[ProxyHandler], state: ProxyState):
        super().__init__(server_address, handler_class)
        self.state = state


def parse_args(argv: Iterable[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Live egress approval proxy for copilot-box")
    parser.add_argument("--listen-host", required=True)
    parser.add_argument("--listen-port", required=True, type=int)
    parser.add_argument("--allowed-hosts-file", required=True, type=Path)
    parser.add_argument("--pending-log-file", required=True, type=Path)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--ready-file", required=True, type=Path)
    return parser.parse_args(list(argv))


def main(argv: Iterable[str]) -> int:
    args = parse_args(argv)
    args.allowed_hosts_file.parent.mkdir(parents=True, exist_ok=True)
    args.pending_log_file.parent.mkdir(parents=True, exist_ok=True)
    args.ready_file.parent.mkdir(parents=True, exist_ok=True)
    state = ProxyState(args.allowed_hosts_file, args.pending_log_file, args.workspace)
    server = ProxyServer((args.listen_host, args.listen_port), ProxyHandler, state)
    args.ready_file.write_text(
        json.dumps(
            {
                "listen_host": server.server_address[0],
                "listen_port": server.server_address[1],
            }
        ),
        encoding="utf-8",
    )
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

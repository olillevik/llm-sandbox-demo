use crate::SessionUiArgs;
use crate::egress::parse_target_spec;
use crate::session::{SessionStore, write_atomic};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UiReady {
    pub(crate) listen_port: u16,
}

pub(crate) fn run_session_ui_command(args: SessionUiArgs) -> Result<i32> {
    if let Some(parent) = args.ready_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let listener =
        TcpListener::bind((args.listen_host.as_str(), args.listen_port)).with_context(|| {
            format!(
                "failed to bind browser companion on {}:{}",
                args.listen_host, args.listen_port
            )
        })?;
    let listen_port = listener
        .local_addr()
        .context("failed to inspect browser companion listener")?
        .port();
    write_atomic(
        &args.ready_file,
        &serde_json::to_vec(&UiReady { listen_port })
            .context("failed to serialize ui ready state")?,
    )?;

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let session_dir = args.session_dir.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_ui_request(stream, &session_dir) {
                        eprintln!("ui server error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("ui accept error: {error}"),
        }
    }

    Ok(0)
}

fn handle_ui_request(mut stream: TcpStream, session_dir: &Path) -> Result<()> {
    configure_stream(&stream)?;
    let store = SessionStore::from_dir(session_dir.to_path_buf());
    let clone = stream.try_clone().context("failed to clone ui stream")?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read ui request line")?;
    if request_line.is_empty() {
        return write_text(&mut stream, 400, "Bad Request", "invalid request\n");
    }
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return write_text(
            &mut stream,
            431,
            "Request Header Fields Too Large",
            "request line too long\n",
        );
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    let mut content_length = 0usize;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read ui header line")?;
        if line.is_empty() {
            return write_text(&mut stream, 400, "Bad Request", "invalid request\n");
        }
        header_bytes += line.len();
        if line.len() > MAX_HEADER_LINE_BYTES || header_bytes > MAX_HEADER_BYTES {
            return write_text(
                &mut stream,
                431,
                "Request Header Fields Too Large",
                "headers too large\n",
            );
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    if content_length > MAX_REQUEST_BODY_BYTES {
        return write_text(&mut stream, 413, "Payload Too Large", "payload too large\n");
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .context("failed to read ui request body")?;
    }

    match (method, target) {
        ("GET", "/") => write_html(&mut stream),
        ("GET", "/api/state") => write_json(&mut stream, &store.load_state()?),
        ("POST", "/api/allow") => {
            handle_target_mutation(&mut stream, &store, &body, SessionStore::allow_target)
        }
        ("POST", "/api/deny") => {
            handle_target_mutation(&mut stream, &store, &body, SessionStore::deny_target)
        }
        ("POST", "/api/dismiss") => {
            handle_target_mutation(&mut stream, &store, &body, SessionStore::dismiss_target)
        }
        _ => write_text(&mut stream, 404, "Not Found", "not found\n"),
    }
}

fn handle_target_mutation(
    stream: &mut TcpStream,
    store: &SessionStore,
    body: &[u8],
    mutate: impl Fn(&SessionStore, &str) -> Result<()>,
) -> Result<()> {
    let target = match parse_target_body(body) {
        Ok(target) => target,
        Err(_) => return write_text(stream, 400, "Bad Request", "invalid destination\n"),
    };
    mutate(store, &target)?;
    write_json(stream, &store.load_state()?)
}

fn parse_target_body(body: &[u8]) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Body {
        target: String,
    }
    let payload: Body = serde_json::from_slice(body).context("failed to parse ui request body")?;
    Ok(parse_target_spec(&payload.target)?.to_string())
}

fn write_html(stream: &mut TcpStream) -> Result<()> {
    let html = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>llm-box approvals</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 0; background: #0b1020; color: #e8ecf3; }
    header { padding: 16px 20px; border-bottom: 1px solid #24304a; background: #111831; }
    h1 { margin: 0 0 6px; font-size: 20px; }
    .meta { color: #9fb0cf; font-size: 14px; }
    .grid { display: grid; grid-template-columns: minmax(0, 1.3fr) minmax(320px, 0.7fr); gap: 16px; padding: 16px; }
    .card { background: #111831; border: 1px solid #24304a; border-radius: 12px; padding: 14px; }
    .card h2 { margin: 0 0 12px; font-size: 16px; }
    .empty { color: #8b97b3; }
    .item { border-top: 1px solid #24304a; padding: 10px 0; }
    .item:first-child { border-top: none; padding-top: 0; }
    .host { font-weight: 600; }
    .meta-line { color: #8b97b3; font-size: 13px; margin-top: 4px; }
    .selectable { user-select: text; -webkit-user-select: text; cursor: text; }
    .inline-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    .host-wrap { min-width: 0; }
    .host-wrap .host { overflow-wrap: anywhere; }
    .actions { display: flex; gap: 8px; flex-shrink: 0; }
    button { background: #3a68ff; color: white; border: none; border-radius: 8px; padding: 8px 10px; margin-right: 0; cursor: pointer; }
    button.secondary { background: #24304a; }
    button.danger { background: #8d2f49; }
    .compact .item { padding: 8px 0; }
    .compact button { padding: 6px 10px; }
  </style>
</head>
<body>
  <header>
    <h1>llm-box approvals</h1>
    <div class="meta" id="meta">Loading…</div>
  </header>
  <div class="grid">
    <section class="card">
      <h2>Pending</h2>
      <div id="pending"></div>
    </section>
    <section class="card">
      <h2>Allowed</h2>
      <div id="allowed"></div>
    </section>
  </div>
  <script>
    let lastStateHash = '';
    async function post(path, target) {
      const response = await fetch(path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ target }),
      });
      if (!response.ok) {
        throw new Error(`request failed with ${response.status}`);
      }
      await refresh();
    }
    function makeEmptyState(text) {
      const el = document.createElement('div');
      el.className = 'empty';
      el.textContent = text;
      return el;
    }
    function makeButton(label, className, onClick) {
      const button = document.createElement('button');
      if (className) button.className = className;
      button.textContent = label;
      button.addEventListener('click', async () => {
        try {
          await onClick();
        } catch (error) {
          document.getElementById('meta').textContent = `Request failed: ${error.message}`;
        }
      });
      return button;
    }
    function renderPending(items) {
      const el = document.getElementById('pending');
      el.replaceChildren();
      if (!items.length) { el.appendChild(makeEmptyState('No blocked destinations')); return; }
      for (const item of items) {
        const wrapper = document.createElement('div');
        wrapper.className = 'item';

        const host = document.createElement('div');
        host.className = 'host selectable';
        host.textContent = item.target;

        const lastSeen = document.createElement('div');
        lastSeen.className = 'meta-line selectable';
        lastSeen.textContent = `Last seen: ${item.last_seen_epoch_nanos}`;

        if (item.connector_endpoint) {
          const endpoint = document.createElement('div');
          endpoint.className = 'meta-line selectable';
          endpoint.textContent = `Connector: ${item.connector_endpoint}`;
          wrapper.append(endpoint);
        }

        const actions = document.createElement('div');
        actions.className = 'meta-line';
        actions.appendChild(makeButton('Allow', '', () => post('/api/allow', item.target)));
        actions.appendChild(makeButton('Dismiss', 'secondary', () => post('/api/dismiss', item.target)));

        wrapper.prepend(host, lastSeen);
        wrapper.append(actions);
        el.appendChild(wrapper);
      }
    }
    function renderAllowed(items) {
      const el = document.getElementById('allowed');
      el.replaceChildren();
      if (!items.length) { el.appendChild(makeEmptyState('No approved destinations')); return; }
      const container = document.createElement('div');
      container.className = 'compact';
      for (const itemData of items) {
        const item = document.createElement('div');
        item.className = 'item';

        const row = document.createElement('div');
        row.className = 'inline-row';

        const wrap = document.createElement('div');
        wrap.className = 'host-wrap selectable';
        const hostEl = document.createElement('div');
        hostEl.className = 'host';
        hostEl.textContent = itemData.target;
        wrap.appendChild(hostEl);
        if (itemData.connector_endpoint) {
          const endpointEl = document.createElement('div');
          endpointEl.className = 'meta-line';
          endpointEl.textContent = `Connector: ${itemData.connector_endpoint}`;
          wrap.appendChild(endpointEl);
        }

        const actions = document.createElement('div');
        actions.className = 'actions';
        actions.appendChild(makeButton('Deny', 'danger', () => post('/api/deny', itemData.target)));

        row.append(wrap, actions);
        item.appendChild(row);
        container.appendChild(item);
      }
      el.appendChild(container);
    }
    async function refresh() {
      const response = await fetch('/api/state');
      if (!response.ok) {
        throw new Error(`refresh failed with ${response.status}`);
      }
      const state = await response.json();
      const selection = window.getSelection ? window.getSelection().toString() : '';
      const nextHash = JSON.stringify(state);
      if (selection) return;
      if (nextHash === lastStateHash) return;
      lastStateHash = nextHash;
      document.getElementById('meta').textContent = `Session ${state.session.session_id} • ${state.session.provider} • ${state.session.workspace}`;
      renderPending(state.pending);
      renderAllowed(state.allowed);
    }
    async function refreshLoop() {
      try {
        await refresh();
      } catch (error) {
        document.getElementById('meta').textContent = `Refresh failed: ${error.message}`;
      }
    }
    refreshLoop();
    setInterval(refreshLoop, 1000);
  </script>
</body>
</html>"#;
    write_response(
        stream,
        200,
        "OK",
        "text/html; charset=utf-8",
        html.as_bytes(),
    )
}

fn write_json<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    write_response(
        stream,
        200,
        "OK",
        "application/json; charset=utf-8",
        &serde_json::to_vec(value).context("failed to serialize ui response")?,
    )
}

fn write_text(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> Result<()> {
    write_response(
        stream,
        status,
        reason,
        "text/plain; charset=utf-8",
        body.as_bytes(),
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .context("failed to write ui response headers")?;
    stream
        .write_all(body)
        .context("failed to write ui response body")
}

fn configure_stream(stream: &TcpStream) -> Result<()> {
    stream
        .set_read_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set ui read timeout")?;
    stream
        .set_write_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set ui write timeout")
}

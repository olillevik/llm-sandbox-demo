use crate::{PendingItem, SessionMeta, SessionUiArgs, UiReady, normalize_host, write_atomic};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;

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
    let clone = stream.try_clone().context("failed to clone ui stream")?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read ui request line")?;
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read ui header line")?;
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

    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .context("failed to read ui request body")?;
    }

    match (method, target) {
        ("GET", "/") => write_html(&mut stream),
        ("GET", "/api/state") => write_json(&mut stream, &load_state(session_dir)?),
        ("POST", "/api/allow") => {
            let host = parse_host_body(&body)?;
            mutate_allowed(session_dir, &host, true)?;
            write_json(&mut stream, &load_state(session_dir)?)
        }
        ("POST", "/api/deny") => {
            let host = parse_host_body(&body)?;
            mutate_allowed(session_dir, &host, false)?;
            write_json(&mut stream, &load_state(session_dir)?)
        }
        ("POST", "/api/dismiss") => {
            let host = parse_host_body(&body)?;
            dismiss_host(session_dir, &host)?;
            write_json(&mut stream, &load_state(session_dir)?)
        }
        _ => write_text(&mut stream, 404, "Not Found", "not found\n"),
    }
}

fn parse_host_body(body: &[u8]) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Body {
        host: String,
    }
    let payload: Body = serde_json::from_slice(body).context("failed to parse ui request body")?;
    normalize_host(&payload.host)
}

#[derive(Serialize)]
struct UiState {
    session: SessionMeta,
    pending: Vec<PendingItem>,
    allowed: Vec<String>,
}

fn load_state(session_dir: &Path) -> Result<UiState> {
    let session: SessionMeta = serde_json::from_str(
        &fs::read_to_string(session_dir.join("session-meta.json")).with_context(|| {
            format!(
                "failed to read {}",
                session_dir.join("session-meta.json").display()
            )
        })?,
    )
    .context("failed to parse session metadata")?;

    let allowed = read_allowed(session_dir)?;
    let pending = read_pending(session_dir, &allowed, &read_dismissed(session_dir)?)?;
    Ok(UiState {
        session,
        pending,
        allowed: allowed.into_iter().collect(),
    })
}

fn read_allowed(session_dir: &Path) -> Result<Vec<String>> {
    let contents =
        fs::read_to_string(session_dir.join("allowed-hosts.txt")).with_context(|| {
            format!(
                "failed to read {}",
                session_dir.join("allowed-hosts.txt").display()
            )
        })?;
    let mut hosts = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(normalize_host)
        .collect::<Result<Vec<_>>>()?;
    hosts.sort();
    hosts.dedup();
    Ok(hosts)
}

fn read_dismissed(session_dir: &Path) -> Result<BTreeMap<String, u64>> {
    let path = session_dir.join("dismissed.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_pending(
    session_dir: &Path,
    allowed: &[String],
    dismissed: &BTreeMap<String, u64>,
) -> Result<Vec<PendingItem>> {
    let allowed = allowed.iter().cloned().collect::<HashSet<_>>();
    let contents = fs::read_to_string(session_dir.join("pending.jsonl")).with_context(|| {
        format!(
            "failed to read {}",
            session_dir.join("pending.jsonl").display()
        )
    })?;
    let mut latest = BTreeMap::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: crate::PendingLogEntry =
            serde_json::from_str(line).context("failed to parse pending log line")?;
        let host = normalize_host(&event.host)?;
        let epoch = event.timestamp.parse::<u64>().unwrap_or(0);
        latest.insert(
            host.clone(),
            PendingItem {
                host,
                port: event.port,
                timestamp: event.timestamp,
                epoch,
            },
        );
    }
    let mut items = latest
        .into_values()
        .filter(|item| !allowed.contains(&item.host))
        .filter(|item| dismissed.get(&item.host).copied().unwrap_or(0) < item.epoch)
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.epoch.cmp(&a.epoch).then_with(|| a.host.cmp(&b.host)));
    Ok(items)
}

fn mutate_allowed(session_dir: &Path, host: &str, allow: bool) -> Result<()> {
    let mut hosts = read_allowed(session_dir)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if allow {
        hosts.insert(normalize_host(host)?);
    } else {
        hosts.remove(&normalize_host(host)?);
    }
    let contents = hosts
        .into_iter()
        .map(|item| format!("{item}\n"))
        .collect::<String>();
    write_atomic(&session_dir.join("allowed-hosts.txt"), contents.as_bytes())
}

fn dismiss_host(session_dir: &Path, host: &str) -> Result<()> {
    let mut dismissed = read_dismissed(session_dir)?;
    dismissed.insert(normalize_host(host)?, crate::current_epoch_seconds());
    let bytes =
        serde_json::to_vec_pretty(&dismissed).context("failed to serialize dismissed state")?;
    write_atomic(
        &session_dir.join("dismissed.json"),
        &[bytes, vec![b'\n']].concat(),
    )
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
    async function post(path, host) {
      await fetch(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ host }) });
      await refresh();
    }
    function renderPending(items) {
      const el = document.getElementById('pending');
      if (!items.length) { el.innerHTML = '<div class="empty">No blocked hosts</div>'; return; }
      el.innerHTML = items.map(item => `
        <div class="item">
          <div class="host selectable">${item.port ? item.host + ':' + item.port : item.host}</div>
          <div class="meta-line selectable">Last seen: ${item.timestamp}</div>
          <div class="meta-line">
            <button onclick="post('/api/allow', '${item.host}')">Allow</button>
            <button class="secondary" onclick="post('/api/dismiss', '${item.host}')">Dismiss</button>
          </div>
        </div>
      `).join('');
    }
    function renderAllowed(items) {
      const el = document.getElementById('allowed');
      if (!items.length) { el.innerHTML = '<div class="empty">No approved hosts</div>'; return; }
      el.innerHTML = `<div class="compact">` + items.map(host => `
        <div class="item">
          <div class="inline-row">
            <div class="host-wrap selectable">
              <div class="host">${host}</div>
            </div>
            <div class="actions">
              <button class="danger" onclick="post('/api/deny', '${host}')">Deny</button>
            </div>
          </div>
        </div>
      `).join('') + `</div>`;
    }
    async function refresh() {
      const state = await (await fetch('/api/state')).json();
      const selection = window.getSelection ? window.getSelection().toString() : '';
      const nextHash = JSON.stringify(state);
      if (selection) return;
      if (nextHash === lastStateHash) return;
      lastStateHash = nextHash;
      document.getElementById('meta').textContent = `Session ${state.session.session_id} • ${state.session.provider} • ${state.session.workspace}`;
      renderPending(state.pending);
      renderAllowed(state.allowed);
    }
    refresh();
    setInterval(refresh, 1000);
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

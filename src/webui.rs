use crate::HubUiArgs;
use crate::egress::{AllowedItem, PendingItem, parse_target_spec};
use crate::session::{SessionMeta, SessionStore, current_epoch_seconds, write_atomic};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::Duration;

const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024;
pub(crate) const UI_HUB_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UiReady {
    pub(crate) listen_port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UiHealth {
    pub(crate) status: String,
    pub(crate) protocol_version: u32,
}

#[derive(Debug, Serialize)]
struct HubState {
    sessions: Vec<HubSession>,
}

#[derive(Debug, Serialize)]
struct HubSession {
    session: SessionMeta,
    pending: Vec<PendingItem>,
    allowed: Vec<AllowedItem>,
    pending_count: usize,
    last_pending_epoch_nanos: u64,
}

#[derive(Debug, Deserialize)]
struct MutationBody {
    session_id: String,
    target: String,
}

pub(crate) fn run_ui_hub_command(args: HubUiArgs) -> Result<i32> {
    if let Some(parent) = args.ready_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.activity_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let listener =
        TcpListener::bind((args.listen_host.as_str(), args.listen_port)).with_context(|| {
            format!(
                "failed to bind approvals hub on {}:{}",
                args.listen_host, args.listen_port
            )
        })?;
    let listen_port = listener
        .local_addr()
        .context("failed to inspect approvals hub listener")?
        .port();
    write_atomic(
        &args.ready_file,
        &serde_json::to_vec(&UiReady { listen_port })
            .context("failed to serialize ui ready state")?,
    )?;

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let sessions_root = args.sessions_root.clone();
                let activity_file = args.activity_file.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_ui_request(stream, &sessions_root, &activity_file) {
                        eprintln!("ui hub error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("ui hub accept error: {error}"),
        }
    }

    Ok(0)
}

fn handle_ui_request(
    mut stream: TcpStream,
    sessions_root: &Path,
    activity_file: &Path,
) -> Result<()> {
    configure_stream(&stream)?;
    if let Err(error) = handle_ui_request_inner(&mut stream, sessions_root, activity_file) {
        let _ = write_text(
            &mut stream,
            500,
            "Internal Server Error",
            "internal error\n",
        );
        return Err(error);
    }
    Ok(())
}

fn handle_ui_request_inner(
    stream: &mut TcpStream,
    sessions_root: &Path,
    activity_file: &Path,
) -> Result<()> {
    let clone = stream.try_clone().context("failed to clone ui stream")?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read ui request line")?;
    if request_line.is_empty() {
        return write_text(stream, 400, "Bad Request", "invalid request\n");
    }
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return write_text(
            stream,
            431,
            "Request Header Fields Too Large",
            "request line too long\n",
        );
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_target = parts.next().unwrap_or("/");
    let route = raw_target.split('?').next().unwrap_or(raw_target);
    if route != "/api/health" {
        mark_ui_activity(activity_file)?;
    }

    let mut content_length = 0usize;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read ui header line")?;
        if line.is_empty() {
            return write_text(stream, 400, "Bad Request", "invalid request\n");
        }
        header_bytes += line.len();
        if line.len() > MAX_HEADER_LINE_BYTES || header_bytes > MAX_HEADER_BYTES {
            return write_text(
                stream,
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
        return write_text(stream, 413, "Payload Too Large", "payload too large\n");
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .context("failed to read ui request body")?;
    }

    match (method, route) {
        ("GET", "/") => write_html(stream),
        ("GET", "/api/health") => write_json(
            stream,
            &UiHealth {
                status: "ok".to_string(),
                protocol_version: UI_HUB_PROTOCOL_VERSION,
            },
        ),
        ("GET", "/api/state") => write_json(stream, &load_hub_state(sessions_root)?),
        ("POST", "/api/allow") => {
            handle_target_mutation(stream, sessions_root, &body, SessionStore::allow_target)
        }
        ("POST", "/api/deny") => {
            handle_target_mutation(stream, sessions_root, &body, SessionStore::deny_target)
        }
        ("POST", "/api/dismiss") => {
            handle_target_mutation(stream, sessions_root, &body, SessionStore::dismiss_target)
        }
        _ => write_text(stream, 404, "Not Found", "not found\n"),
    }
}

fn handle_target_mutation(
    stream: &mut TcpStream,
    sessions_root: &Path,
    body: &[u8],
    mutate: impl Fn(&SessionStore, &str) -> Result<()>,
) -> Result<()> {
    let payload = match parse_mutation_body(body) {
        Ok(payload) => payload,
        Err(_) => return write_text(stream, 400, "Bad Request", "invalid destination\n"),
    };
    let store = match session_dir_for_id(sessions_root, &payload.session_id) {
        Ok(session_dir) => SessionStore::from_dir(session_dir),
        Err(_) => return write_text(stream, 404, "Not Found", "unknown session\n"),
    };
    mutate(&store, &payload.target)?;
    write_json(stream, &load_hub_state(sessions_root)?)
}

fn parse_mutation_body(body: &[u8]) -> Result<MutationBody> {
    let payload: MutationBody =
        serde_json::from_slice(body).context("failed to parse ui request body")?;
    Ok(MutationBody {
        session_id: validate_session_id(&payload.session_id)?.to_string(),
        target: parse_target_spec(&payload.target)?.to_string(),
    })
}

fn validate_session_id(session_id: &str) -> Result<&str> {
    let mut components = Path::new(session_id).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) if !session_id.is_empty() => Ok(session_id),
        _ => bail!("invalid session id"),
    }
}

fn session_dir_for_id(sessions_root: &Path, session_id: &str) -> Result<PathBuf> {
    let session_id = validate_session_id(session_id)?;
    let session_dir = sessions_root.join(session_id);
    if !session_dir.join("session-meta.json").is_file() {
        bail!("unknown session `{session_id}`");
    }
    Ok(session_dir)
}

fn load_hub_state(sessions_root: &Path) -> Result<HubState> {
    if !sessions_root.exists() {
        return Ok(HubState {
            sessions: Vec::new(),
        });
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(sessions_root)
        .with_context(|| format!("failed to read {}", sessions_root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", sessions_root.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let session_dir = entry.path();
        if !session_dir.join("session-meta.json").is_file() {
            continue;
        }
        let store = SessionStore::from_dir(session_dir);
        let is_active = match store.is_active_session() {
            Ok(is_active) => is_active,
            Err(error) => {
                eprintln!(
                    "ui hub: skipping session with unreadable activity {}: {error:#}",
                    store.session_dir().display()
                );
                continue;
            }
        };
        if !is_active {
            continue;
        }
        let state = match store.load_state() {
            Ok(state) => state,
            Err(error) => {
                eprintln!(
                    "ui hub: skipping unreadable session {}: {error:#}",
                    store.session_dir().display()
                );
                continue;
            }
        };
        let last_pending_epoch_nanos = state
            .pending
            .iter()
            .map(|item| item.epoch)
            .max()
            .unwrap_or(0);
        sessions.push(HubSession {
            pending_count: state.pending.len(),
            last_pending_epoch_nanos,
            session: state.session,
            pending: state.pending,
            allowed: state.allowed,
        });
    }
    sessions.sort_by(|a, b| {
        b.last_pending_epoch_nanos
            .cmp(&a.last_pending_epoch_nanos)
            .then_with(|| b.pending_count.cmp(&a.pending_count))
            .then_with(|| {
                b.session
                    .last_started_epoch
                    .cmp(&a.session.last_started_epoch)
            })
            .then_with(|| a.session.session_id.cmp(&b.session.session_id))
    });
    Ok(HubState { sessions })
}

fn write_html(stream: &mut TcpStream) -> Result<()> {
    let html = r##"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>llm-box</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 0; background: #0b1020; color: #e8ecf3; }
    header { padding: 16px 20px; border-bottom: 1px solid #24304a; background: #111831; }
    h1 { margin: 0 0 6px; font-size: 20px; }
    h2, h3 { margin: 0 0 12px; }
    .meta { color: #9fb0cf; font-size: 14px; }
    .layout { display: grid; grid-template-columns: minmax(300px, 360px) minmax(0, 1fr); gap: 16px; padding: 16px; }
    .stack { display: grid; gap: 16px; }
    .card { background: #111831; border: 1px solid #24304a; border-radius: 12px; padding: 14px; }
    .empty { color: #8b97b3; }
    .session-list { display: grid; gap: 10px; }
    .session-row { width: 100%; text-align: left; border: 1px solid #24304a; border-radius: 12px; background: #0f1730; color: inherit; padding: 12px; cursor: pointer; }
    .session-row.selected { border-color: #5b86ff; box-shadow: 0 0 0 1px #5b86ff inset; }
    .session-row.unseen-pending { border-color: #c96b2c; background: #1a1422; }
    .session-row.seen-pending { border-color: #5b5772; }
    .session-top { display: grid; gap: 8px; }
    .session-name { font-weight: 700; overflow-wrap: anywhere; }
    .labels { display: grid; gap: 6px; justify-items: start; }
    .label { display: inline-flex; align-items: center; gap: 6px; border-radius: 999px; padding: 3px 10px; font-size: 12px; font-weight: 700; }
    .label.unseen-pending { background: #6d2c1b; color: #ffd9c0; }
    .label.seen-pending { background: #343047; color: #d7d2f4; }
    .session-meta { color: #9fb0cf; font-size: 13px; margin-top: 6px; overflow-wrap: anywhere; }
    .detail-grid { display: grid; grid-template-columns: minmax(0, 1fr) minmax(320px, 0.9fr); gap: 16px; }
    .item { border-top: 1px solid #24304a; padding: 10px 0; }
    .item:first-child { border-top: none; padding-top: 0; }
    .host { font-weight: 600; overflow-wrap: anywhere; }
    .meta-line { color: #8b97b3; font-size: 13px; margin-top: 4px; overflow-wrap: anywhere; }
    .selectable { user-select: text; -webkit-user-select: text; cursor: text; }
    .inline-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    .host-wrap { min-width: 0; }
    .actions { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 8px; }
    button.action { background: #3a68ff; color: white; border: none; border-radius: 8px; padding: 8px 10px; cursor: pointer; }
    button.action.secondary { background: #24304a; }
    button.action.danger { background: #8d2f49; }
    @media (max-width: 960px) {
      .layout { grid-template-columns: 1fr; }
      .detail-grid { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <header>
    <h1>llm-box</h1>
    <div class="meta" id="meta">Loading…</div>
  </header>
  <div class="layout">
    <aside class="card">
      <h2>Sessions</h2>
      <div id="sessions"></div>
    </aside>
    <main class="stack">
      <section class="detail-grid">
        <div class="card">
          <h3>Pending</h3>
          <div id="pending"></div>
        </div>
        <div class="card">
          <h3>Allowed</h3>
          <div id="allowed"></div>
        </div>
      </section>
    </main>
  </div>
  <script>
    const SEEN_KEY = 'llmBoxSeenPendingEpochBySession';
    const FOCUS_KEY = 'llmBoxFocusedSessionId';
    let lastStateHash = '';
    let currentState = { sessions: [] };
    let selectedSessionId = new URLSearchParams(window.location.search).get('session') || localStorage.getItem(FOCUS_KEY) || '';
    let seenMap = loadSeenMap();

    function loadSeenMap() {
      try {
        const parsed = JSON.parse(localStorage.getItem(SEEN_KEY) || '{}');
        return parsed && typeof parsed === 'object' ? parsed : {};
      } catch (_) {
        return {};
      }
    }

    function saveSeenMap() {
      localStorage.setItem(SEEN_KEY, JSON.stringify(seenMap));
    }

    function parseEpoch(value) {
      const parsed = Number(value);
      return Number.isFinite(parsed) ? parsed : 0;
    }

    function workspaceName(workspace) {
      const trimmed = workspace.replace(/[\\/]+$/, '');
      const parts = trimmed.split(/[\\/]/).filter(Boolean);
      return parts.length ? parts[parts.length - 1] : workspace;
    }

    function shortSessionId(sessionId) {
      return sessionId.length > 12 ? sessionId.slice(0, 12) : sessionId;
    }

    function sessionAttention(sessionData) {
      const sessionId = sessionData.session.session_id;
      const seenEpoch = parseEpoch(seenMap[sessionId] || 0);
      const newestEpoch = sessionData.pending.reduce(
        (max, item) => Math.max(max, parseEpoch(item.last_seen_epoch_nanos)),
        0
      );
      const unseenCount = sessionData.pending.filter(
        item => parseEpoch(item.last_seen_epoch_nanos) > seenEpoch
      ).length;
      const state = unseenCount > 0 ? 'unseen-pending' : sessionData.pending.length > 0 ? 'seen-pending' : '';
      return { state, seenEpoch, newestEpoch, unseenCount };
    }

    function sessionLabels(sessionData, attention) {
      const labels = [];
      if (sessionData.pending.length > 0) {
        labels.push({ text: `Pending ${sessionData.pending.length}`, className: 'seen-pending' });
      }
      if (attention.unseenCount > 0) {
        labels.push({ text: `Unread ${attention.unseenCount}`, className: 'unseen-pending' });
      }
      return labels;
    }

    function sortedSessions(sessions) {
      return [...sessions].sort((left, right) => {
        const a = sessionAttention(left);
        const b = sessionAttention(right);
        const leftRank = a.unseenCount > 0 ? 2 : left.pending.length > 0 ? 1 : 0;
        const rightRank = b.unseenCount > 0 ? 2 : right.pending.length > 0 ? 1 : 0;
        return (
          rightRank - leftRank ||
          b.unseenCount - a.unseenCount ||
          b.newestEpoch - a.newestEpoch ||
          right.pending.length - left.pending.length ||
          right.session.last_started_epoch - left.session.last_started_epoch ||
          left.session.session_id.localeCompare(right.session.session_id)
        );
      });
    }

    function persistSelectedSession() {
      if (selectedSessionId) {
        localStorage.setItem(FOCUS_KEY, selectedSessionId);
      } else {
        localStorage.removeItem(FOCUS_KEY);
      }
      const params = new URLSearchParams(window.location.search);
      if (selectedSessionId) {
        params.set('session', selectedSessionId);
      } else {
        params.delete('session');
      }
      const query = params.toString();
      history.replaceState(null, '', query ? `${window.location.pathname}?${query}` : window.location.pathname);
    }

    function markSessionSeen(sessionData) {
      if (!sessionData) return;
      const attention = sessionAttention(sessionData);
      if (attention.newestEpoch > parseEpoch(seenMap[sessionData.session.session_id] || 0)) {
        seenMap[sessionData.session.session_id] = attention.newestEpoch;
        saveSeenMap();
      }
    }

    function makeEmptyState(text) {
      const el = document.createElement('div');
      el.className = 'empty';
      el.textContent = text;
      return el;
    }

    function makeButton(label, className, onClick) {
      const button = document.createElement('button');
      button.className = `action ${className || ''}`.trim();
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

    async function post(path, sessionId, target) {
      const response = await fetch(path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: sessionId, target }),
      });
      if (!response.ok) {
        throw new Error(`request failed with ${response.status}`);
      }
      currentState = await response.json();
      renderState();
    }

    function renderSessions(sessions) {
      const root = document.getElementById('sessions');
      root.replaceChildren();
      if (!sessions.length) {
        root.appendChild(makeEmptyState('No active sessions'));
        return;
      }
      const list = document.createElement('div');
      list.className = 'session-list';
      for (const sessionData of sessions) {
        const sessionId = sessionData.session.session_id;
        const attention = sessionAttention(sessionData);
        const row = document.createElement('button');
        row.type = 'button';
        row.className = `session-row${attention.state ? ` ${attention.state}` : ''}${sessionId === selectedSessionId ? ' selected' : ''}`;
        row.addEventListener('click', () => {
          selectedSessionId = sessionId;
          persistSelectedSession();
          markSessionSeen(sessionData);
          renderState();
        });

        const top = document.createElement('div');
        top.className = 'session-top';
        const name = document.createElement('div');
        name.className = 'session-name';
        name.textContent = workspaceName(sessionData.session.workspace);
        top.appendChild(name);
        const labels = sessionLabels(sessionData, attention);
        if (labels.length) {
          const labelsEl = document.createElement('div');
          labelsEl.className = 'labels';
          for (const labelData of labels) {
            const label = document.createElement('div');
            label.className = `label ${labelData.className}`;
            label.textContent = labelData.text;
            labelsEl.appendChild(label);
          }
          top.appendChild(labelsEl);
        }

        const meta = document.createElement('div');
        meta.className = 'session-meta';
        meta.textContent = `Session ${shortSessionId(sessionId)} • ${sessionData.session.provider}`;

        const detail = document.createElement('div');
        detail.className = 'session-meta';
        if (sessionData.pending.length) {
          const newest = sessionData.pending.reduce(
            (latest, item) => (parseEpoch(item.last_seen_epoch_nanos) > parseEpoch(latest.last_seen_epoch_nanos) ? item : latest),
            sessionData.pending[0]
          );
          detail.textContent = `Blocked ${newest.target}`;
        } else {
          detail.textContent = sessionData.session.workspace;
        }

        row.append(top, meta, detail);
        list.appendChild(row);
      }
      root.appendChild(list);
    }

    function renderPending(sessionData) {
      const root = document.getElementById('pending');
      root.replaceChildren();
      if (!sessionData) {
        root.appendChild(makeEmptyState('Select a session to inspect pending approvals'));
        return;
      }
      if (!sessionData.pending.length) {
        root.appendChild(makeEmptyState('No blocked destinations'));
        return;
      }
      for (const item of sessionData.pending) {
        const wrapper = document.createElement('div');
        wrapper.className = 'item';

        const host = document.createElement('div');
        host.className = 'host selectable';
        host.textContent = item.target;

        const lastSeen = document.createElement('div');
        lastSeen.className = 'meta-line selectable';
        lastSeen.textContent = `Last seen: ${item.last_seen_epoch_nanos}`;

        wrapper.append(host, lastSeen);
        if (item.connector_endpoint) {
          const endpoint = document.createElement('div');
          endpoint.className = 'meta-line selectable';
          endpoint.textContent = `Connector: ${item.connector_endpoint}`;
          wrapper.append(endpoint);
        }

        const actions = document.createElement('div');
        actions.className = 'actions';
        actions.appendChild(makeButton('Allow', '', () => post('/api/allow', sessionData.session.session_id, item.target)));
        actions.appendChild(makeButton('Dismiss', 'secondary', () => post('/api/dismiss', sessionData.session.session_id, item.target)));
        wrapper.append(actions);
        root.appendChild(wrapper);
      }
    }

    function renderAllowed(sessionData) {
      const root = document.getElementById('allowed');
      root.replaceChildren();
      if (!sessionData) {
        root.appendChild(makeEmptyState('Select a session to inspect approved destinations'));
        return;
      }
      if (!sessionData.allowed.length) {
        root.appendChild(makeEmptyState('No approved destinations'));
        return;
      }
      for (const itemData of sessionData.allowed) {
        const item = document.createElement('div');
        item.className = 'item';

        const host = document.createElement('div');
        host.className = 'host selectable';
        host.textContent = itemData.target;
        item.appendChild(host);

        if (itemData.connector_endpoint) {
          const endpoint = document.createElement('div');
          endpoint.className = 'meta-line selectable';
          endpoint.textContent = `Connector: ${itemData.connector_endpoint}`;
          item.appendChild(endpoint);
        }

        const actions = document.createElement('div');
        actions.className = 'actions';
        actions.appendChild(makeButton('Deny', 'danger', () => post('/api/deny', sessionData.session.session_id, itemData.target)));
        item.appendChild(actions);
        root.appendChild(item);
      }
    }

    function renderDetails(sessionData) {
      renderPending(sessionData);
      renderAllowed(sessionData);
    }

    function renderMeta(sessions) {
      const count = sessions.length;
      document.getElementById('meta').textContent =
        `${count} active session${count === 1 ? '' : 's'}`;
    }

    function renderState() {
      const sessions = sortedSessions(currentState.sessions);
      if (!sessions.some(sessionData => sessionData.session.session_id === selectedSessionId)) {
        selectedSessionId = sessions[0] ? sessions[0].session.session_id : '';
      }
      persistSelectedSession();
      const selected = sessions.find(sessionData => sessionData.session.session_id === selectedSessionId) || null;
      if (selected) {
        markSessionSeen(selected);
      }
      const rerenderedSessions = sortedSessions(currentState.sessions);
      renderMeta(rerenderedSessions);
      renderSessions(rerenderedSessions);
      renderDetails(
        rerenderedSessions.find(sessionData => sessionData.session.session_id === selectedSessionId) || null
      );
    }

    async function refresh() {
      const response = await fetch('/api/state');
      if (!response.ok) {
        throw new Error(`refresh failed with ${response.status}`);
      }
      const nextState = await response.json();
      if (window.getSelection && window.getSelection().toString()) {
        return;
      }
      const nextHash = JSON.stringify(nextState);
      if (nextHash === lastStateHash) {
        return;
      }
      lastStateHash = nextHash;
      currentState = nextState;
      renderState();
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
</html>"##;
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

fn mark_ui_activity(activity_file: &Path) -> Result<()> {
    write_atomic(
        activity_file,
        format!("{}\n", current_epoch_seconds()).as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionMeta;
    use std::net::Shutdown;

    #[test]
    fn hub_state_loads_multiple_sessions() {
        let root = unique_test_dir("hub-state-loads-multiple-sessions");
        let sessions_root = root.join("sessions");
        let first_dir = sessions_root.join("session-a");
        let second_dir = sessions_root.join("session-b");
        write_session_fixture(
            &first_dir,
            &SessionMeta {
                session_id: "session-a".to_string(),
                workspace: "/tmp/workspace-a".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 10,
                last_invocation: vec!["--resume".to_string()],
            },
            true,
            "https://allowed.example:443\n",
            "{\"event_epoch_nanos\":\"5\",\"kind\":\"https\",\"host\":\"pending.example\",\"port\":443}\n",
        );
        write_session_fixture(
            &second_dir,
            &SessionMeta {
                session_id: "session-b".to_string(),
                workspace: "/tmp/workspace-b".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 20,
                last_invocation: Vec::new(),
            },
            true,
            "",
            "",
        );

        let state = load_hub_state(&sessions_root).unwrap();

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[0].session.session_id, "session-a");
        assert_eq!(state.sessions[0].pending_count, 1);
        assert_eq!(state.sessions[0].allowed.len(), 1);
        assert_eq!(state.sessions[1].session.session_id, "session-b");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hub_state_skips_broken_sessions() {
        let root = unique_test_dir("hub-state-skips-broken-sessions");
        let sessions_root = root.join("sessions");
        let valid_dir = sessions_root.join("session-valid");
        let broken_dir = sessions_root.join("session-broken");
        write_session_fixture(
            &valid_dir,
            &SessionMeta {
                session_id: "session-valid".to_string(),
                workspace: "/tmp/workspace-valid".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 10,
                last_invocation: Vec::new(),
            },
            true,
            "https://allowed.example:443\n",
            "",
        );
        fs::create_dir_all(&broken_dir).unwrap();
        fs::write(broken_dir.join("session-meta.json"), "{not-json}\n").unwrap();
        fs::write(broken_dir.join("allowed-targets.txt"), "").unwrap();
        fs::write(broken_dir.join("connectors.json"), "[]\n").unwrap();
        fs::write(broken_dir.join("pending-events.jsonl"), "").unwrap();
        fs::write(broken_dir.join("dismissed.json"), "{}\n").unwrap();

        let state = load_hub_state(&sessions_root).unwrap();

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].session.session_id, "session-valid");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hub_state_excludes_inactive_sessions() {
        let root = unique_test_dir("hub-state-excludes-inactive-sessions");
        let sessions_root = root.join("sessions");
        let active_dir = sessions_root.join("session-active");
        let inactive_dir = sessions_root.join("session-inactive");
        write_session_fixture(
            &active_dir,
            &SessionMeta {
                session_id: "session-active".to_string(),
                workspace: "/tmp/workspace-active".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 20,
                last_invocation: Vec::new(),
            },
            true,
            "",
            "",
        );
        write_session_fixture(
            &inactive_dir,
            &SessionMeta {
                session_id: "session-inactive".to_string(),
                workspace: "/tmp/workspace-inactive".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 10,
                last_invocation: Vec::new(),
            },
            false,
            "",
            "",
        );

        let state = load_hub_state(&sessions_root).unwrap();

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].session.session_id, "session-active");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_dir_for_id_rejects_nested_paths() {
        let root = unique_test_dir("hub-session-id-validation");
        fs::create_dir_all(root.join("sessions")).unwrap();

        let error = session_dir_for_id(&root.join("sessions"), "../escape").unwrap_err();

        assert!(error.to_string().contains("invalid session id"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handle_ui_request_returns_http_500_on_internal_failure() {
        let root = unique_test_dir("hub-http-500");
        let sessions_root = root.join("sessions-root-file");
        let activity_file = root.join("ui-activity");
        fs::create_dir_all(&root).unwrap();
        fs::write(&sessions_root, "not-a-directory\n").unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_ui_request(stream, &sessions_root, &activity_file).unwrap_err()
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .write_all(b"GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();

        let error = server.join().unwrap();
        assert!(error.to_string().contains("failed to read"));
        assert!(response.starts_with("HTTP/1.1 500 Internal Server Error"));
        assert!(response.contains("internal error"));

        let _ = fs::remove_dir_all(root);
    }

    fn write_session_fixture(
        session_dir: &Path,
        meta: &SessionMeta,
        active: bool,
        allowed_targets: &str,
        pending_events: &str,
    ) {
        fs::create_dir_all(session_dir).unwrap();
        fs::write(
            session_dir.join("session-meta.json"),
            serde_json::to_vec(meta).unwrap(),
        )
        .unwrap();
        fs::write(session_dir.join("allowed-targets.txt"), allowed_targets).unwrap();
        fs::write(session_dir.join("connectors.json"), "[]\n").unwrap();
        fs::write(session_dir.join("pending-events.jsonl"), pending_events).unwrap();
        fs::write(session_dir.join("dismissed.json"), "{}\n").unwrap();
        if active {
            fs::write(
                session_dir.join("active-process.json"),
                serde_json::to_vec(&serde_json::json!({
                    "pid": std::process::id(),
                    "started_epoch": current_epoch_seconds(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "llm-box-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}

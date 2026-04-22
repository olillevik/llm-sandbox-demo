use crate::BrokerCommandArgs;
use crate::egress::{
    ConnectorMapping, EgressKind, EgressTarget, PendingLogEntry, ensure_connector_mapping,
    parse_target_spec, read_allowed_targets_file, read_connector_mappings_file,
    serialize_connector_mappings, split_host_port,
};
use crate::session::{current_epoch_nanos, with_file_lock};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BrokerReady {
    pub(crate) listen_port: u16,
}

pub(crate) fn run_broker_command(args: BrokerCommandArgs) -> Result<i32> {
    if let Some(parent) = args.allowed_targets_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.pending_events_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.connectors_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.broker_ready_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let listener =
        TcpListener::bind((args.listen_host.as_str(), args.listen_port)).with_context(|| {
            format!(
                "failed to bind broker on {}:{}",
                args.listen_host, args.listen_port
            )
        })?;
    let listen_port = listener
        .local_addr()
        .context("failed to inspect broker listener")?
        .port();
    let ready = BrokerReady { listen_port };
    fs::write(
        &args.broker_ready_file,
        serde_json::to_vec(&ready).context("failed to serialize broker ready state")?,
    )
    .with_context(|| format!("failed to write {}", args.broker_ready_file.display()))?;

    let state = Arc::new(BrokerState {
        allowed_targets_file: args.allowed_targets_file,
        connectors_file: args.connectors_file,
        pending_events_file: args.pending_events_file,
        host_loopback_alias: args.host_loopback_alias,
        active_tunnels: Mutex::new(HashMap::new()),
        active_connector_ports: Mutex::new(HashSet::new()),
        next_tunnel_id: AtomicU64::new(1),
    });
    spawn_disallowed_tunnel_reaper(Arc::clone(&state));
    spawn_connector_reconciler(Arc::clone(&state));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, state) {
                        eprintln!("broker error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("broker accept error: {error}"),
        }
    }

    Ok(0)
}

struct BrokerState {
    allowed_targets_file: PathBuf,
    connectors_file: PathBuf,
    pending_events_file: PathBuf,
    host_loopback_alias: Option<String>,
    active_tunnels: Mutex<HashMap<String, Vec<ActiveTunnel>>>,
    active_connector_ports: Mutex<HashSet<u16>>,
    next_tunnel_id: AtomicU64,
}

struct ActiveTunnel {
    id: u64,
    client: TcpStream,
    upstream: TcpStream,
}

impl BrokerState {
    fn allowed_targets(&self) -> Result<HashSet<EgressTarget>> {
        if !self.allowed_targets_file.exists() {
            return Ok(HashSet::new());
        }
        Ok(read_allowed_targets_file(&self.allowed_targets_file)?
            .into_iter()
            .collect())
    }

    fn is_allowed_target(&self, target: &EgressTarget) -> Result<bool> {
        Ok(self.allowed_targets()?.contains(target))
    }

    fn connector_mappings(&self) -> Result<Vec<ConnectorMapping>> {
        read_connector_mappings_file(&self.connectors_file)
    }

    fn ensure_connector_for_target(&self, target: &EgressTarget) -> Result<ConnectorMapping> {
        with_file_lock(&self.connectors_file, || {
            let mut mappings = self.connector_mappings()?;
            let mapping = ensure_connector_mapping(&mut mappings, target)?;
            let bytes = serialize_connector_mappings(&mappings)?;
            fs::write(&self.connectors_file, bytes)
                .with_context(|| format!("failed to write {}", self.connectors_file.display()))?;
            Ok(mapping)
        })
    }

    fn log_blocked(&self, target: &EgressTarget) -> Result<()> {
        with_file_lock(&self.pending_events_file, || {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.pending_events_file)
                .with_context(|| {
                    format!("failed to open {}", self.pending_events_file.display())
                })?;
            let record = PendingLogEntry {
                event_epoch_nanos: current_epoch_nanos().to_string(),
                kind: target.kind,
                host: target.host.clone(),
                port: target.port,
            };
            serde_json::to_writer(&mut file, &record)
                .context("failed to write blocked log record")?;
            writeln!(file).context("failed to finish blocked log record")?;
            Ok(())
        })
    }

    fn connect_upstream(&self, host: &str, port: u16) -> Result<TcpStream> {
        let connect_host = if is_loopback_host(host) {
            self.host_loopback_alias
                .as_deref()
                .unwrap_or(host)
                .to_string()
        } else {
            host.to_string()
        };
        let stream = TcpStream::connect((connect_host.as_str(), port))
            .with_context(|| format!("failed to connect to {host}:{port}"))?;
        configure_stream(&stream)?;
        Ok(stream)
    }

    fn register_tunnel(&self, key: &str, client: &TcpStream, upstream: &TcpStream) -> Result<u64> {
        let id = self.next_tunnel_id.fetch_add(1, Ordering::Relaxed);
        let tunnel = ActiveTunnel {
            id,
            client: client
                .try_clone()
                .context("failed to clone client tunnel stream")?,
            upstream: upstream
                .try_clone()
                .context("failed to clone upstream tunnel stream")?,
        };
        let mut tunnels = self
            .active_tunnels
            .lock()
            .map_err(|_| anyhow::anyhow!("active tunnel lock poisoned"))?;
        tunnels.entry(key.to_string()).or_default().push(tunnel);
        Ok(id)
    }

    fn unregister_tunnel(&self, key: &str, id: u64) -> Result<()> {
        let mut tunnels = self
            .active_tunnels
            .lock()
            .map_err(|_| anyhow::anyhow!("active tunnel lock poisoned"))?;
        if let Some(entries) = tunnels.get_mut(key) {
            entries.retain(|entry| entry.id != id);
            if entries.is_empty() {
                tunnels.remove(key);
            }
        }
        Ok(())
    }

    fn close_disallowed_tunnels(&self, allowed_keys: &HashSet<String>) -> Result<()> {
        let disallowed = {
            let mut tunnels = self
                .active_tunnels
                .lock()
                .map_err(|_| anyhow::anyhow!("active tunnel lock poisoned"))?;
            let hosts = tunnels
                .keys()
                .filter(|host| !allowed_keys.contains(*host))
                .cloned()
                .collect::<Vec<_>>();
            let mut disallowed = Vec::new();
            for host in hosts {
                if let Some(entries) = tunnels.remove(&host) {
                    disallowed.extend(entries);
                }
            }
            disallowed
        };

        for tunnel in disallowed {
            let _ = tunnel.client.shutdown(Shutdown::Both);
            let _ = tunnel.upstream.shutdown(Shutdown::Both);
        }
        Ok(())
    }

    fn mark_connector_port_active(&self, port: u16) -> Result<bool> {
        let mut ports = self
            .active_connector_ports
            .lock()
            .map_err(|_| anyhow::anyhow!("active connector lock poisoned"))?;
        Ok(ports.insert(port))
    }
}

fn spawn_disallowed_tunnel_reaper(state: Arc<BrokerState>) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            match state.allowed_targets() {
                Ok(allowed_targets) => {
                    let allowed_keys = allowed_targets
                        .into_iter()
                        .map(|target| target.to_string())
                        .collect::<HashSet<_>>();
                    if let Err(error) = state.close_disallowed_tunnels(&allowed_keys) {
                        eprintln!("broker tunnel reaper error: {error:#}");
                    }
                }
                Err(error) => eprintln!("broker allowlist reload error: {error:#}"),
            }
        }
    });
}

fn spawn_connector_reconciler(state: Arc<BrokerState>) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            match state.connector_mappings() {
                Ok(mappings) => {
                    for mapping in mappings {
                        match state.mark_connector_port_active(mapping.listen_port) {
                            Ok(false) => continue,
                            Ok(true) => {
                                let state = Arc::clone(&state);
                                thread::spawn(move || run_connector_listener(state, mapping));
                            }
                            Err(error) => {
                                eprintln!("broker connector tracking error: {error:#}");
                            }
                        }
                    }
                }
                Err(error) => eprintln!("broker connector reload error: {error:#}"),
            }
        }
    });
}

#[derive(Debug)]
struct ParsedRequest {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle_client(client: TcpStream, state: Arc<BrokerState>) -> Result<()> {
    configure_stream(&client)?;
    let mut client = client;
    let request = match read_request(&client) {
        Ok(request) => request,
        Err(error) => {
            let _ = send_error(&mut client, 400, "Bad Request", "invalid broker request");
            return Err(error);
        }
    };
    if request.method.eq_ignore_ascii_case("POST") && request.target == "/__endpoint" {
        handle_connector_control(client, state, request)
    } else if request.method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(client, state, &request.target)
    } else {
        handle_plain_http(client, state, request)
    }
}

fn run_connector_listener(state: Arc<BrokerState>, mapping: ConnectorMapping) {
    let listener = match TcpListener::bind(("0.0.0.0", mapping.listen_port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "broker connector bind error for {} on {}: {error}",
                mapping.target, mapping.listen_port
            );
            return;
        }
    };
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let mapping = mapping.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_connector_client(stream, state, mapping) {
                        eprintln!("broker connector error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("broker connector accept error: {error}"),
        }
    }
}

fn handle_connector_client(
    client: TcpStream,
    state: Arc<BrokerState>,
    mapping: ConnectorMapping,
) -> Result<()> {
    configure_stream(&client)?;
    let key = mapping.target.to_string();
    if !state.is_allowed_target(&mapping.target)? {
        state.log_blocked(&mapping.target)?;
        return Ok(());
    }
    let upstream = state.connect_upstream(&mapping.target.host, mapping.target.port)?;
    let tunnel_id = state.register_tunnel(&key, &client, &upstream)?;
    let result = tunnel(client, upstream);
    let unregister_result = state.unregister_tunnel(&key, tunnel_id);
    result?;
    unregister_result
}

fn handle_connector_control(
    mut client: TcpStream,
    state: Arc<BrokerState>,
    request: ParsedRequest,
) -> Result<()> {
    #[derive(Deserialize)]
    struct ConnectorRequest {
        target: String,
    }

    #[derive(Serialize)]
    struct ConnectorResponse {
        target: String,
        endpoint: String,
    }

    let payload: ConnectorRequest = serde_json::from_slice(&request.body)
        .context("failed to parse connector control payload")?;
    let target = parse_target_spec(&payload.target)?;
    let mapping = state.ensure_connector_for_target(&target)?;
    let body = serde_json::to_vec(&ConnectorResponse {
        target: mapping.target.to_string(),
        endpoint: mapping.endpoint(),
    })
    .context("failed to serialize connector control response")?;
    client
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .context("failed to write connector control headers")?;
    client
        .write_all(&body)
        .context("failed to write connector control body")
}

fn read_request(client: &TcpStream) -> Result<ParsedRequest> {
    let clone = client
        .try_clone()
        .context("failed to clone client stream")?;
    let mut reader = BufReader::new(clone);

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read request line")?;
    if request_line.trim().is_empty() {
        bail!("empty request line");
    }
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        bail!("request line too long");
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing request method")?.to_string();
    let target = parts.next().context("missing request target")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read header line")?;
        if line.is_empty() {
            bail!("unexpected end of headers");
        }
        header_bytes += line.len();
        if line.len() > MAX_HEADER_LINE_BYTES {
            bail!("header line too long");
        }
        if header_bytes > MAX_HEADER_BYTES {
            bail!("headers too large");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed.split_once(':').context("invalid header line")?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let content_length = header_value(&headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        bail!("request body too large");
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .context("failed to read request body")?;
    }

    Ok(ParsedRequest {
        method,
        target,
        version,
        headers,
        body,
    })
}

fn handle_connect(mut client: TcpStream, state: Arc<BrokerState>, target: &str) -> Result<()> {
    let target = parse_connect_target(target)?;
    let key = target.to_string();
    if !state.is_allowed_target(&target)? {
        state.log_blocked(&target)?;
        send_error(
            &mut client,
            403,
            "Forbidden",
            &format!("{key} is not approved"),
        )?;
        return Ok(());
    }

    let upstream = state.connect_upstream(&target.host, target.port)?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\nConnection: close\r\n\r\n")
        .context("failed to acknowledge CONNECT")?;
    let tunnel_id = state.register_tunnel(&key, &client, &upstream)?;
    let result = tunnel(client, upstream);
    let unregister_result = state.unregister_tunnel(&key, tunnel_id);
    result?;
    unregister_result
}

fn handle_plain_http(
    mut client: TcpStream,
    state: Arc<BrokerState>,
    request: ParsedRequest,
) -> Result<()> {
    let (target, path, host_header) = parse_http_target(&request.target, &request.headers)?;
    let key = target.to_string();
    if !state.is_allowed_target(&target)? {
        state.log_blocked(&target)?;
        send_error(
            &mut client,
            403,
            "Forbidden",
            &format!("{key} is not approved"),
        )?;
        return Ok(());
    }

    let mut upstream = state.connect_upstream(&target.host, target.port)?;
    upstream
        .write_all(format!("{} {} {}\r\n", request.method, path, request.version).as_bytes())
        .context("failed to write request line upstream")?;

    let hop_by_hop = hop_by_hop_headers(&request.headers);
    for (name, value) in &request.headers {
        let lower = name.to_ascii_lowercase();
        if lower == "host" || hop_by_hop.contains(lower.as_str()) {
            continue;
        }
        upstream
            .write_all(format!("{name}: {value}\r\n").as_bytes())
            .context("failed to forward request header")?;
    }
    upstream
        .write_all(format!("Host: {host_header}\r\nConnection: close\r\n\r\n").as_bytes())
        .context("failed to finalize upstream headers")?;
    if !request.body.is_empty() {
        upstream
            .write_all(&request.body)
            .context("failed to forward request body")?;
    }
    upstream.flush().ok();

    io::copy(&mut upstream, &mut client).context("failed to relay upstream response")?;
    Ok(())
}

fn parse_connect_target(target: &str) -> Result<EgressTarget> {
    if let Some((host, port)) = target.rsplit_once(':') {
        return EgressTarget::new(
            EgressKind::Https,
            host,
            port.parse().context("invalid CONNECT port")?,
        );
    }
    EgressTarget::new(EgressKind::Https, target, 443)
}

fn parse_http_target(
    target: &str,
    headers: &[(String, String)],
) -> Result<(EgressTarget, String, String)> {
    let host_header = header_value(headers, "host").unwrap_or_default();
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, remainder) = split_once_or(rest, '/', (rest, ""));
        let (host, port) = split_host_port(authority, 80)?;
        let path = if remainder.is_empty() {
            "/".to_string()
        } else {
            format!("/{remainder}")
        };
        let host_header = if authority.is_empty() {
            format_host_header(&host, port, 80)
        } else {
            authority.to_string()
        };
        return Ok((
            EgressTarget::new(EgressKind::Http, &host, port)?,
            path,
            host_header,
        ));
    }

    let (host, port) = split_host_port(&host_header, 80)?;
    let path = if target.is_empty() { "/" } else { target }.to_string();
    Ok((
        EgressTarget::new(EgressKind::Http, &host, port)?,
        path,
        format_host_header(&host, port, 80),
    ))
}

fn format_host_header(host: &str, port: u16, default_port: u16) -> String {
    if port == default_port {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

fn split_once_or<'a>(
    value: &'a str,
    needle: char,
    default: (&'a str, &'a str),
) -> (&'a str, &'a str) {
    value.split_once(needle).unwrap_or(default)
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn hop_by_hop_headers(headers: &[(String, String)]) -> HashSet<String> {
    let mut values = HOP_BY_HOP_HEADERS
        .iter()
        .map(|value| value.to_string())
        .collect::<HashSet<_>>();
    if let Some(connection) = header_value(headers, "connection") {
        for token in connection.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if !token.is_empty() {
                values.insert(token);
            }
        }
    }
    values
}

fn send_error(stream: &mut TcpStream, status: u16, reason: &str, message: &str) -> Result<()> {
    let body = format!("{message}\n");
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .context("failed to write broker error response")
}

fn configure_stream(stream: &TcpStream) -> Result<()> {
    stream
        .set_read_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set read timeout")?;
    stream
        .set_write_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set write timeout")
}

fn tunnel(client: TcpStream, upstream: TcpStream) -> Result<()> {
    let mut client_reader = client
        .try_clone()
        .context("failed to clone client stream")?;
    let mut upstream_writer = upstream
        .try_clone()
        .context("failed to clone upstream stream")?;
    let copy_client = thread::spawn(move || {
        let _ = io::copy(&mut client_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
    });

    let mut upstream_reader = upstream;
    let mut client_writer = client;
    let _ = io::copy(&mut upstream_reader, &mut client_writer);
    let _ = client_writer.shutdown(Shutdown::Write);
    let _ = copy_client.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let client = TcpStream::connect(addr).expect("connect client");
        let server = listener.accept().expect("accept connection").0;
        (client, server)
    }

    #[test]
    fn close_disallowed_tunnels_shuts_down_registered_streams() {
        let temp_dir =
            std::env::temp_dir().join(format!("llm-box-proxy-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let state = BrokerState {
            allowed_targets_file: temp_dir.join("allowed-targets.txt"),
            connectors_file: temp_dir.join("connectors.json"),
            pending_events_file: temp_dir.join("pending-events.jsonl"),
            host_loopback_alias: None,
            active_tunnels: Mutex::new(HashMap::new()),
            active_connector_ports: Mutex::new(HashSet::new()),
            next_tunnel_id: AtomicU64::new(1),
        };
        fs::write(&state.allowed_targets_file, "https://allowed.example:443\n")
            .expect("write allowlist");

        let (mut client_peer, client_proxy_side) = connected_pair();
        let (mut upstream_peer, upstream_proxy_side) = connected_pair();
        client_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client timeout");
        upstream_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set upstream timeout");

        let tunnel_id = state
            .register_tunnel(
                "https://denied.example:443",
                &client_proxy_side,
                &upstream_proxy_side,
            )
            .expect("register tunnel");
        state
            .close_disallowed_tunnels(&HashSet::from([String::from(
                "https://allowed.example:443",
            )]))
            .expect("close disallowed tunnels");
        state
            .unregister_tunnel("https://denied.example:443", tunnel_id)
            .expect("unregister tunnel");

        let mut buffer = [0_u8; 1];
        let client_bytes = client_peer
            .read(&mut buffer)
            .expect("read client peer after shutdown");
        let upstream_bytes = upstream_peer
            .read(&mut buffer)
            .expect("read upstream peer after shutdown");
        assert_eq!(client_bytes, 0);
        assert_eq!(upstream_bytes, 0);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn read_request_rejects_invalid_header_line() {
        let (mut client, server) = connected_pair();
        client
            .write_all(b"GET / HTTP/1.1\r\ninvalid-header\r\n\r\n")
            .expect("write malformed request");

        let error = read_request(&server).unwrap_err();
        assert!(error.to_string().contains("invalid header line"));
    }

    #[test]
    fn parse_connect_target_rejects_invalid_port() {
        let error = parse_connect_target("example.com:not-a-port").unwrap_err();
        assert!(error.to_string().contains("invalid CONNECT port"));
    }

    #[test]
    fn loopback_detection_matches_supported_hosts() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn ensure_connector_for_target_reuses_mapping() {
        let temp_dir =
            std::env::temp_dir().join(format!("llm-box-proxy-connector-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let state = BrokerState {
            allowed_targets_file: temp_dir.join("allowed-targets.txt"),
            connectors_file: temp_dir.join("connectors.json"),
            pending_events_file: temp_dir.join("pending-events.jsonl"),
            host_loopback_alias: None,
            active_tunnels: Mutex::new(HashMap::new()),
            active_connector_ports: Mutex::new(HashSet::new()),
            next_tunnel_id: AtomicU64::new(1),
        };
        fs::write(&state.connectors_file, "[]\n").expect("write connectors");

        let target = EgressTarget::new(EgressKind::Tcp, "db.example", 5432).unwrap();
        let first = state.ensure_connector_for_target(&target).unwrap();
        let second = state.ensure_connector_for_target(&target).unwrap();

        assert_eq!(first.listen_port, second.listen_port);
        assert_eq!(
            first.endpoint(),
            crate::egress::connector_endpoint(first.listen_port)
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}

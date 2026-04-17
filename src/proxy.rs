use crate::{PendingLogEntry, ProxyCommandArgs, ProxyReady, current_epoch_seconds, normalize_host};
use anyhow::{Context, Result, bail};
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

pub(crate) fn run_proxy_command(args: ProxyCommandArgs) -> Result<i32> {
    if let Some(parent) = args.allowed_hosts_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.pending_log_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = args.ready_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let listener =
        TcpListener::bind((args.listen_host.as_str(), args.listen_port)).with_context(|| {
            format!(
                "failed to bind proxy on {}:{}",
                args.listen_host, args.listen_port
            )
        })?;
    let listen_port = listener
        .local_addr()
        .context("failed to inspect proxy listener")?
        .port();
    let ready = ProxyReady { listen_port };
    fs::write(
        &args.ready_file,
        serde_json::to_vec(&ready).context("failed to serialize proxy ready state")?,
    )
    .with_context(|| format!("failed to write {}", args.ready_file.display()))?;

    let state = Arc::new(ProxyState {
        allowed_hosts_file: args.allowed_hosts_file,
        pending_log_file: args.pending_log_file,
        workspace: args.workspace,
        log_lock: Mutex::new(()),
        active_tunnels: Mutex::new(HashMap::new()),
        next_tunnel_id: AtomicU64::new(1),
    });
    spawn_disallowed_tunnel_reaper(Arc::clone(&state));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, state) {
                        eprintln!("proxy error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("proxy accept error: {error}"),
        }
    }

    Ok(0)
}

struct ProxyState {
    allowed_hosts_file: PathBuf,
    pending_log_file: PathBuf,
    workspace: String,
    log_lock: Mutex<()>,
    active_tunnels: Mutex<HashMap<String, Vec<ActiveTunnel>>>,
    next_tunnel_id: AtomicU64,
}

struct ActiveTunnel {
    id: u64,
    client: TcpStream,
    upstream: TcpStream,
}

impl ProxyState {
    fn allowed_hosts(&self) -> Result<HashSet<String>> {
        if !self.allowed_hosts_file.exists() {
            return Ok(HashSet::new());
        }
        let contents = fs::read_to_string(&self.allowed_hosts_file)
            .with_context(|| format!("failed to read {}", self.allowed_hosts_file.display()))?;
        Ok(contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(normalize_host)
            .collect::<Result<HashSet<_>>>()?)
    }

    fn is_allowed(&self, host: &str) -> Result<bool> {
        Ok(self.allowed_hosts()?.contains(&normalize_host(host)?))
    }

    fn log_blocked(&self, host: &str, port: Option<u16>) -> Result<()> {
        let _guard = self
            .log_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy log lock poisoned"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.pending_log_file)
            .with_context(|| format!("failed to open {}", self.pending_log_file.display()))?;
        let record = PendingLogEntry {
            timestamp: current_epoch_seconds().to_string(),
            host: normalize_host(host)?,
            port,
        };
        serde_json::to_writer(&mut file, &record).context("failed to write blocked log record")?;
        writeln!(file).context("failed to finish blocked log record")?;
        let _ = &self.workspace;
        Ok(())
    }

    fn register_tunnel(&self, host: &str, client: &TcpStream, upstream: &TcpStream) -> Result<u64> {
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
        tunnels
            .entry(normalize_host(host)?)
            .or_default()
            .push(tunnel);
        Ok(id)
    }

    fn unregister_tunnel(&self, host: &str, id: u64) -> Result<()> {
        let mut tunnels = self
            .active_tunnels
            .lock()
            .map_err(|_| anyhow::anyhow!("active tunnel lock poisoned"))?;
        let host = normalize_host(host)?;
        if let Some(entries) = tunnels.get_mut(&host) {
            entries.retain(|entry| entry.id != id);
            if entries.is_empty() {
                tunnels.remove(&host);
            }
        }
        Ok(())
    }

    fn close_disallowed_tunnels(&self, allowed_hosts: &HashSet<String>) -> Result<()> {
        let disallowed = {
            let mut tunnels = self
                .active_tunnels
                .lock()
                .map_err(|_| anyhow::anyhow!("active tunnel lock poisoned"))?;
            let hosts = tunnels
                .keys()
                .filter(|host| !allowed_hosts.contains(*host))
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
}

fn spawn_disallowed_tunnel_reaper(state: Arc<ProxyState>) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(250));
            match state.allowed_hosts() {
                Ok(allowed_hosts) => {
                    if let Err(error) = state.close_disallowed_tunnels(&allowed_hosts) {
                        eprintln!("proxy tunnel reaper error: {error:#}");
                    }
                }
                Err(error) => eprintln!("proxy allowlist reload error: {error:#}"),
            }
        }
    });
}

struct ParsedRequest {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle_client(client: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let request = read_request(&client)?;
    if request.method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(client, state, &request.target)
    } else {
        handle_plain_http(client, state, request)
    }
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
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing request method")?.to_string();
    let target = parts.next().context("missing request target")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read header line")?;
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

fn handle_connect(mut client: TcpStream, state: Arc<ProxyState>, target: &str) -> Result<()> {
    let (host, port) = parse_connect_target(target)?;
    if !state.is_allowed(&host)? {
        state.log_blocked(&host, Some(port))?;
        send_error(
            &mut client,
            403,
            "Forbidden",
            &format!("{host} is not approved"),
        )?;
        return Ok(());
    }

    let upstream = TcpStream::connect((host.as_str(), port))
        .with_context(|| format!("failed to connect to {host}:{port}"))?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\nConnection: close\r\n\r\n")
        .context("failed to acknowledge CONNECT")?;
    let tunnel_id = state.register_tunnel(&host, &client, &upstream)?;
    let result = tunnel(client, upstream);
    let unregister_result = state.unregister_tunnel(&host, tunnel_id);
    result?;
    unregister_result
}

fn handle_plain_http(
    mut client: TcpStream,
    state: Arc<ProxyState>,
    request: ParsedRequest,
) -> Result<()> {
    let (host, port, path, host_header) = parse_http_target(&request.target, &request.headers)?;
    if !state.is_allowed(&host)? {
        state.log_blocked(&host, Some(port))?;
        send_error(
            &mut client,
            403,
            "Forbidden",
            &format!("{host} is not approved"),
        )?;
        return Ok(());
    }

    let mut upstream = TcpStream::connect((host.as_str(), port))
        .with_context(|| format!("failed to connect to {host}:{port}"))?;
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

fn parse_connect_target(target: &str) -> Result<(String, u16)> {
    if let Some((host, port)) = target.rsplit_once(':') {
        return Ok((
            normalize_host(host)?,
            port.parse().context("invalid CONNECT port")?,
        ));
    }
    Ok((normalize_host(target)?, 443))
}

fn parse_http_target(
    target: &str,
    headers: &[(String, String)],
) -> Result<(String, u16, String, String)> {
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
        return Ok((host, port, path, host_header));
    }

    let (host, port) = split_host_port(&host_header, 80)?;
    let path = if target.is_empty() { "/" } else { target }.to_string();
    Ok((
        host.clone(),
        port,
        path,
        format_host_header(&host, port, 80),
    ))
}

fn split_host_port(value: &str, default_port: u16) -> Result<(String, u16)> {
    let trimmed = value.trim();
    if let Some(stripped) = trimmed.strip_prefix('[') {
        let (host, remainder) = stripped.split_once(']').context("invalid bracketed host")?;
        let port = if let Some(port) = remainder.strip_prefix(':') {
            port.parse().context("invalid port")?
        } else {
            default_port
        };
        return Ok((normalize_host(host)?, port));
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if !host.contains(':') {
            return Ok((normalize_host(host)?, port.parse().context("invalid port")?));
        }
    }
    Ok((normalize_host(trimmed)?, default_port))
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
        .context("failed to write proxy error response")
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
        let state = ProxyState {
            allowed_hosts_file: temp_dir.join("allowed-hosts.txt"),
            pending_log_file: temp_dir.join("pending.jsonl"),
            workspace: "test".to_string(),
            log_lock: Mutex::new(()),
            active_tunnels: Mutex::new(HashMap::new()),
            next_tunnel_id: AtomicU64::new(1),
        };
        fs::write(&state.allowed_hosts_file, "allowed.example\n").expect("write allowlist");

        let (mut client_peer, client_proxy_side) = connected_pair();
        let (mut upstream_peer, upstream_proxy_side) = connected_pair();
        client_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client timeout");
        upstream_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set upstream timeout");

        let tunnel_id = state
            .register_tunnel("denied.example", &client_proxy_side, &upstream_proxy_side)
            .expect("register tunnel");
        state
            .close_disallowed_tunnels(&HashSet::from([String::from("allowed.example")]))
            .expect("close disallowed tunnels");
        state
            .unregister_tunnel("denied.example", tunnel_id)
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
}

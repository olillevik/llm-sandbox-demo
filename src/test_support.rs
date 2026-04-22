use crate::ServeStaticArgs;
use crate::session::workspace_key;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::Duration;

const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;

pub(crate) fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("failed to bind ephemeral port")?;
    Ok(listener
        .local_addr()
        .context("failed to inspect ephemeral port")?
        .port())
}

pub(crate) fn latest_session_dir(workspace: &Path, root: &Path) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let workspace_dir = root.join("workspaces").join(workspace_key(&workspace));
    let session_id =
        fs::read_to_string(workspace_dir.join("latest-session")).with_context(|| {
            format!(
                "failed to read latest-session in {}",
                workspace_dir.display()
            )
        })?;
    Ok(root.join("sessions").join(session_id.trim()))
}

pub(crate) fn workspace_home_dir(workspace: &Path, root: &Path) -> PathBuf {
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    root.join("workspaces")
        .join(workspace_key(&workspace))
        .join("home")
}

pub(crate) fn serve_static_command(args: ServeStaticArgs) -> Result<i32> {
    let listener =
        TcpListener::bind((args.listen_host.as_str(), args.listen_port)).with_context(|| {
            format!(
                "failed to bind static server on {}:{}",
                args.listen_host, args.listen_port
            )
        })?;
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let directory = args.directory.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_static_request(stream, &directory) {
                        eprintln!("static server error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("static server accept error: {error}"),
        }
    }
    Ok(0)
}

fn handle_static_request(mut stream: TcpStream, directory: &Path) -> Result<()> {
    configure_stream(&stream)?;
    let clone = stream
        .try_clone()
        .context("failed to clone static server stream")?;
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("failed to read static server request line")?;
    if request_line.is_empty() {
        return write_response(&mut stream, 400, "Bad Request", b"invalid request\n");
    }
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return write_response(
            &mut stream,
            431,
            "Request Header Fields Too Large",
            b"request line too long\n",
        );
    }
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read static server header line")?;
        if line.is_empty() {
            return write_response(&mut stream, 400, "Bad Request", b"invalid request\n");
        }
        header_bytes += line.len();
        if line.len() > MAX_HEADER_LINE_BYTES || header_bytes > MAX_HEADER_BYTES {
            return write_response(
                &mut stream,
                431,
                "Request Header Fields Too Large",
                b"headers too large\n",
            );
        }
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
    }

    if method != "GET" && method != "HEAD" {
        return write_response(
            &mut stream,
            405,
            "Method Not Allowed",
            b"method not allowed\n",
        );
    }

    let file_path = resolve_path(directory, path)?;
    if !file_path.exists() {
        return write_response(&mut stream, 404, "Not Found", b"not found\n");
    }
    let body =
        fs::read(&file_path).with_context(|| format!("failed to read {}", file_path.display()))?;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .context("failed to write static response headers")?;
    if method != "HEAD" {
        stream
            .write_all(&body)
            .context("failed to write static response body")?;
    }
    Ok(())
}

fn resolve_path(root: &Path, request_path: &str) -> Result<PathBuf> {
    let relative = request_path.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("invalid path traversal attempt");
    }
    Ok(root.join(path))
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &[u8]) -> Result<()> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .context("failed to write response headers")?;
    stream
        .write_all(body)
        .context("failed to write response body")
}

fn configure_stream(stream: &TcpStream) -> Result<()> {
    stream
        .set_read_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set static server read timeout")?;
    stream
        .set_write_timeout(Some(STREAM_IO_TIMEOUT))
        .context("failed to set static server write timeout")
}

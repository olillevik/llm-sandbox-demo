use crate::config::AppConfig;
use crate::session::current_epoch_seconds;
use crate::webui;
use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct ApprovalsHub {
    base_url: String,
    started_now: bool,
}

impl ApprovalsHub {
    pub(crate) fn maybe_open(config: &AppConfig, focus_session: Option<&str>) -> Result<()> {
        if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
            return Ok(());
        }
        if env::var_os("LLM_BOX_NO_BROWSER").is_some() {
            return Ok(());
        }
        let hub = Self::ensure_started(config)?;
        let url = hub.url(focus_session);
        eprintln!("llm-box approvals hub: {url}");
        if hub.started_now || !ui_recently_active(config) {
            if let Err(error) = open_browser(&url) {
                eprintln!(
                    "llm-box: failed to open approvals hub automatically; open {url} manually ({error:#})"
                );
            }
        } else if let Some(session_id) = focus_session {
            eprintln!("llm-box session awaiting approvals: {session_id} ({url})");
        }
        Ok(())
    }

    pub(crate) fn url(&self, focus_session: Option<&str>) -> String {
        match focus_session {
            Some(session_id) => format!("{}?session={session_id}", self.base_url),
            None => self.base_url.clone(),
        }
    }

    pub(crate) fn ensure_started(config: &AppConfig) -> Result<Self> {
        let ready_file = config.ui_ready_file();
        if let Some(info) = read_ui_ready(&ready_file) {
            if ui_server_responding(info.listen_port) {
                return Ok(Self {
                    base_url: format!("http://127.0.0.1:{}/", info.listen_port),
                    started_now: false,
                });
            }
        }
        let _ = fs::remove_file(&ready_file);
        let executable = env::current_exe().context("failed to resolve current executable")?;
        let mut child = Command::new(executable)
            .arg("__ui-hub")
            .args([
                "--listen-host",
                "127.0.0.1",
                "--listen-port",
                "0",
                "--sessions-root",
            ])
            .arg(config.sessions_root())
            .arg("--ready-file")
            .arg(&ready_file)
            .arg("--activity-file")
            .arg(config.ui_activity_file())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start approvals hub")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(info) = read_ui_ready(&ready_file) {
                if ui_server_responding(info.listen_port) {
                    return Ok(Self {
                        base_url: format!("http://127.0.0.1:{}/", info.listen_port),
                        started_now: true,
                    });
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!("approvals hub failed to become ready");
            }
            if child
                .try_wait()
                .context("failed to poll approvals hub")?
                .is_some()
            {
                bail!("approvals hub exited before becoming ready");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub(crate) fn open_browser(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    } else if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    } else {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    let status = command.status().context("failed to launch browser")?;
    if !status.success() {
        bail!("browser launcher exited with {status}");
    }
    Ok(())
}

fn read_ui_ready(path: &Path) -> Option<webui::UiReady> {
    let payload = fs::read_to_string(path).ok()?;
    serde_json::from_str(&payload).ok()
}

fn ui_server_responding(listen_port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", listen_port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    if stream
        .write_all(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    let Ok(_) = stream.read_to_end(&mut response) else {
        return false;
    };
    ui_health_matches(&response)
}

fn ui_health_matches(response: &[u8]) -> bool {
    let Ok(response) = String::from_utf8(response.to_vec()) else {
        return false;
    };
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return false;
    };
    if !head.starts_with("HTTP/1.1 200") {
        return false;
    }
    let Ok(payload) = serde_json::from_str::<webui::UiHealth>(body) else {
        return false;
    };
    payload.status == "ok" && payload.protocol_version == webui::UI_HUB_PROTOCOL_VERSION
}

fn ui_recently_active(config: &AppConfig) -> bool {
    const UI_ACTIVITY_FRESH_FOR_SECS: u64 = 5;

    let Ok(raw) = fs::read_to_string(config.ui_activity_file()) else {
        return false;
    };
    let Ok(last_seen) = raw.trim().parse::<u64>() else {
        return false;
    };
    current_epoch_seconds().saturating_sub(last_seen) <= UI_ACTIVITY_FRESH_FOR_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn ui_recently_active_is_true_for_fresh_heartbeat() {
        let root = unique_test_dir("ui-recently-active-fresh");
        let config = AppConfig::for_tests(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            config.ui_activity_file(),
            format!("{}\n", current_epoch_seconds()),
        )
        .unwrap();

        assert!(ui_recently_active(&config));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ui_recently_active_is_false_for_stale_heartbeat() {
        let root = unique_test_dir("ui-recently-active-stale");
        let config = AppConfig::for_tests(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            config.ui_activity_file(),
            format!("{}\n", current_epoch_seconds().saturating_sub(60)),
        )
        .unwrap();

        assert!(!ui_recently_active(&config));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ui_health_matches_current_protocol() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{{\"status\":\"ok\",\"protocol_version\":{}}}",
            webui::UI_HUB_PROTOCOL_VERSION
        );

        assert!(ui_health_matches(response.as_bytes()));
    }

    #[test]
    fn ui_health_rejects_legacy_text_response() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok\n";

        assert!(!ui_health_matches(response.as_bytes()));
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

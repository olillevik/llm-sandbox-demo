use crate::broker;
use crate::config::AppConfig;
use crate::egress::{BROKER_INTERNAL_HOST, parse_target_spec};
use crate::session::{SessionContext, workspace_key, write_atomic};
use crate::ui::ApprovalsHub;
use anyhow::{Context, Result, bail};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const REPO_OVERLAY_DOCKERFILE: &str = ".llm-box/Dockerfile";
const REPO_OVERLAY_BASE_IMAGE_ARG: &str = "LLM_BOX_BASE_IMAGE";
const PROVIDER_AUTH_ENV_KEYS: &[&str] = &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];
const GH_AUTH_REUSE_TIP: &str = "llm-box: no reusable GitHub token found. Tip: run `gh auth login` on the host to let llm-box reuse your GitHub auth.";
const MANAGED_COPILOT_CONFIG_DIR: &str = "/home/copilot/.llm-box/providers/copilot";
const LEGACY_AUTH_MARKER_FILE: &str = "llm-box-authenticated.json";
const WORKSPACE_STATE_VERSION: u32 = 1;
const WORKSPACE_STATE_VERSION_FILE: &str = "workspace-state.json";
const OPTIONAL_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const HOST_GH_AUTH_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const INIT_IMAGE_TEMPLATE: &str = r#"ARG LLM_BOX_BASE_IMAGE
FROM ${LLM_BOX_BASE_IMAGE}

USER root
# RUN apt-get update \
#   && apt-get install -y --no-install-recommends gh \
#   && rm -rf /var/lib/apt/lists/*
USER copilot
"#;

pub(crate) const BROKER_LISTEN_PORT: u16 = 3128;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GhAuthStatus {
    Unavailable,
    Authenticated(String),
    NotAuthenticated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigDirArg {
    Absent,
    Present(String),
    MissingValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderAuthEnv {
    passthrough_env: Vec<(String, String)>,
    secret_env_vars: Vec<String>,
    user_tip: Option<&'static str>,
}

pub(crate) fn build_image(config: &AppConfig) -> Result<()> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    build_copilot_image(config, &runtime, &workspace).map(|_| ())
}

pub(crate) fn init_repo_overlay_dockerfile(workspace: &Path) -> Result<PathBuf> {
    let dockerfile = workspace.join(REPO_OVERLAY_DOCKERFILE);
    if dockerfile.exists() {
        bail!(
            "repo overlay Dockerfile already exists: {}",
            dockerfile.display()
        );
    }
    let parent = dockerfile
        .parent()
        .context("repo overlay Dockerfile path had no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    write_atomic(&dockerfile, INIT_IMAGE_TEMPLATE.as_bytes())?;
    Ok(dockerfile)
}

pub(crate) fn run_copilot(config: &AppConfig, args: &[String]) -> Result<i32> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    let image_name = ensure_copilot_image(config, &runtime, &workspace)?;
    let workspace_home = workspace_home_for(config, &workspace);
    migrate_workspace_home_state(&workspace_home)?;
    let explicit_auth_env = collect_passthrough_env(PROVIDER_AUTH_ENV_KEYS);
    let gh_auth_status = if explicit_auth_env.is_empty() {
        query_host_gh_auth_status()
    } else {
        GhAuthStatus::Unavailable
    };
    let provider_auth = resolve_provider_auth_env(explicit_auth_env, gh_auth_status);
    let image_uses_copilot = image_entrypoint_uses_copilot(&runtime, &image_name)?;
    let invocation_args = if image_uses_copilot {
        inject_managed_config_dir(args)
    } else {
        args.to_vec()
    };
    let session = SessionContext::new_session(config, workspace, args)?;
    let _active_session = session.mark_active()?;
    let network = SessionNetwork::create(&runtime, session.session_id())?;
    let broker = BrokerProcess::start(&runtime, &image_name, &session, &network)?;
    ApprovalsHub::maybe_open(config, Some(session.session_id()))?;

    let tty_mode = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        TtyMode::Interactive
    } else {
        TtyMode::StdinOnly
    };

    let uid = current_command_output("id", ["-u"])?;
    let gid = current_command_output("id", ["-g"])?;
    let secret_env_vars = if provider_auth.secret_env_vars.is_empty() || !image_uses_copilot {
        Vec::new()
    } else {
        provider_auth.secret_env_vars.clone()
    };
    if should_emit_gh_auth_tip(args) {
        if let Some(tip) = provider_auth.user_tip {
            eprintln!("{tip}");
        }
    }
    let plan = build_copilot_run_plan(
        &image_name,
        &session,
        &network,
        &uid,
        &gid,
        tty_mode,
        &invocation_args,
        &provider_auth.passthrough_env,
        &secret_env_vars,
        config.shared_copilot_skills_dir(),
    );
    let mut command = plan.command(&runtime);

    let status = command
        .status()
        .context("failed to launch container runtime")?;
    drop(broker);
    drop(network);
    Ok(status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 }))
}

pub(crate) fn request_connector_endpoint_from_broker(target: &str) -> Result<(String, String)> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        target: &'a str,
    }

    #[derive(serde::Deserialize)]
    struct Response {
        target: String,
        endpoint: String,
    }

    let canonical = parse_target_spec(target)?.to_string();
    let body = serde_json::to_vec(&Request { target: &canonical })
        .context("failed to serialize broker endpoint request")?;
    let mut stream = TcpStream::connect((BROKER_INTERNAL_HOST, BROKER_LISTEN_PORT))
        .context("failed to connect to llm-box broker")?;
    stream
        .write_all(
            format!(
                "POST /__endpoint HTTP/1.1\r\nHost: {BROKER_INTERNAL_HOST}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .context("failed to write broker endpoint request headers")?;
    stream
        .write_all(&body)
        .context("failed to write broker endpoint request body")?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("failed to read broker endpoint response")?;
    let response = String::from_utf8(response).context("broker endpoint response was not utf-8")?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("invalid broker endpoint response")?;
    if !head.starts_with("HTTP/1.1 200") {
        bail!("broker endpoint request failed");
    }
    let payload: Response =
        serde_json::from_str(body).context("failed to parse broker endpoint response body")?;
    Ok((payload.target, payload.endpoint))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TtyMode {
    Interactive,
    StdinOnly,
}

impl TtyMode {
    fn runtime_args(self) -> &'static [&'static str] {
        match self {
            Self::Interactive => &["-it"],
            Self::StdinOnly => &["-i"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerRunPlan {
    args: Vec<String>,
}

impl ContainerRunPlan {
    fn command(&self, runtime: &str) -> Command {
        let mut command = Command::new(runtime);
        command.args(&self.args);
        command
    }
}

fn collect_passthrough_env(keys: &[&str]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| {
            env::var_os(key).map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

fn resolve_provider_auth_env(
    explicit_env: Vec<(String, String)>,
    gh_auth_status: GhAuthStatus,
) -> ProviderAuthEnv {
    if !explicit_env.is_empty() {
        let secret_env_vars = explicit_env
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        return ProviderAuthEnv {
            passthrough_env: explicit_env,
            secret_env_vars,
            user_tip: None,
        };
    }

    match gh_auth_status {
        GhAuthStatus::Authenticated(token) => ProviderAuthEnv {
            passthrough_env: vec![("COPILOT_GITHUB_TOKEN".to_string(), token)],
            secret_env_vars: vec!["COPILOT_GITHUB_TOKEN".to_string()],
            user_tip: None,
        },
        GhAuthStatus::NotAuthenticated => ProviderAuthEnv {
            passthrough_env: Vec::new(),
            secret_env_vars: Vec::new(),
            user_tip: Some(GH_AUTH_REUSE_TIP),
        },
        GhAuthStatus::Unavailable => ProviderAuthEnv {
            passthrough_env: Vec::new(),
            secret_env_vars: Vec::new(),
            user_tip: None,
        },
    }
}

fn query_host_gh_auth_status() -> GhAuthStatus {
    if !command_exists("gh") {
        return GhAuthStatus::Unavailable;
    }
    match try_optional_command_output(
        "gh",
        ["auth", "token"],
        &[
            ("GH_PROMPT_DISABLED", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
        ],
        HOST_GH_AUTH_LOOKUP_TIMEOUT,
    ) {
        Ok(Some(token)) => GhAuthStatus::Authenticated(token),
        Ok(None) => GhAuthStatus::NotAuthenticated,
        Err(_) => GhAuthStatus::Unavailable,
    }
}

fn should_emit_gh_auth_tip(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "-v" | "--version"))
    {
        return false;
    }
    !matches!(
        args.first().map(String::as_str),
        Some("help" | "init" | "login" | "mcp" | "plugin" | "update" | "version")
    )
}

fn config_dir_arg(args: &[String]) -> ConfigDirArg {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--config-dir=") {
            return ConfigDirArg::Present(value.to_string());
        }
        if arg == "--config-dir" {
            return match iter.next() {
                Some(value) => ConfigDirArg::Present(value.clone()),
                None => ConfigDirArg::MissingValue,
            };
        }
    }
    ConfigDirArg::Absent
}

fn inject_managed_config_dir(args: &[String]) -> Vec<String> {
    match config_dir_arg(args) {
        ConfigDirArg::Absent => {
            let mut normalized = vec![
                "--config-dir".to_string(),
                MANAGED_COPILOT_CONFIG_DIR.to_string(),
            ];
            normalized.extend(args.iter().cloned());
            normalized
        }
        ConfigDirArg::Present(_) | ConfigDirArg::MissingValue => args.to_vec(),
    }
}

fn build_copilot_run_plan(
    image_name: &str,
    session: &SessionContext,
    network: &SessionNetwork,
    uid: &str,
    gid: &str,
    tty_mode: TtyMode,
    invocation_args: &[String],
    passthrough_env: &[(String, String)],
    secret_env_vars: &[String],
    shared_skills_dir: Option<&Path>,
) -> ContainerRunPlan {
    let mut args = vec!["run".to_string()];
    args.extend(tty_mode.runtime_args().iter().map(|arg| (*arg).to_string()));
    args.push("--rm".to_string());
    args.extend([
        "--user".to_string(),
        format!("{uid}:{gid}"),
        "--workdir".to_string(),
        "/workspace".to_string(),
        "--network".to_string(),
        network.internal_name().to_string(),
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "-e".to_string(),
        "HOME=/home/copilot".to_string(),
        "-e".to_string(),
        format!("LLM_BOX_SESSION_ID={}", session.session_id()),
    ]);
    for (key, value) in passthrough_env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    let proxy_url = format!("http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}");
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        args.push("-e".to_string());
        args.push(format!("{key}={proxy_url}"));
    }
    args.push("-e".to_string());
    args.push(format!(
        "NO_PROXY=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"
    ));
    args.push("-e".to_string());
    args.push(format!(
        "no_proxy=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"
    ));
    args.push("-v".to_string());
    args.push(format!(
        "{}:/home/copilot",
        session.workspace_home().display()
    ));
    if let Some(shared_skills_dir) = shared_skills_dir {
        args.push("-v".to_string());
        args.push(format!(
            "{}:/home/copilot/.copilot/skills:ro",
            shared_skills_dir.display()
        ));
    }
    args.push("-v".to_string());
    args.push(format!("{}:/workspace", session.workspace().display()));
    args.push(image_name.to_string());
    if !secret_env_vars.is_empty() {
        args.push(format!("--secret-env-vars={}", secret_env_vars.join(",")));
    }
    args.extend(invocation_args.iter().cloned());
    ContainerRunPlan { args }
}

fn build_broker_run_plan(
    runtime: &str,
    image_name: &str,
    session: &SessionContext,
    network: &SessionNetwork,
    loopback_alias: &str,
    add_host_gateway_alias: bool,
) -> ContainerRunPlan {
    let container_name = format!("llm-box-broker-{}", session.session_id());
    let session_mount = format!("{}:/llm-box/session", session.session_dir().display());
    let broker_port = BROKER_LISTEN_PORT.to_string();
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name,
        "--network".to_string(),
        network.internal_name().to_string(),
        "--network-alias".to_string(),
        BROKER_INTERNAL_HOST.to_string(),
        "-v".to_string(),
        session_mount,
        "--entrypoint".to_string(),
        "llm-box".to_string(),
    ];
    if runtime == "docker" && add_host_gateway_alias {
        args.push("--add-host=host.docker.internal:host-gateway".to_string());
    }
    args.extend([
        image_name.to_string(),
        "__broker".to_string(),
        "--listen-host".to_string(),
        "0.0.0.0".to_string(),
        "--listen-port".to_string(),
        broker_port,
        "--allowed-targets-file".to_string(),
        "/llm-box/session/allowed-targets.txt".to_string(),
        "--pending-events-file".to_string(),
        "/llm-box/session/pending-events.jsonl".to_string(),
        "--connectors-file".to_string(),
        "/llm-box/session/connectors.json".to_string(),
        "--broker-ready-file".to_string(),
        "/llm-box/session/broker-ready.json".to_string(),
        "--host-loopback-alias".to_string(),
        loopback_alias.to_string(),
    ]);
    ContainerRunPlan { args }
}

fn ensure_copilot_image(config: &AppConfig, runtime: &str, workspace: &Path) -> Result<String> {
    if !image_exists(runtime, config.image_name())?
        || !image_supports_broker(runtime, config.image_name())?
    {
        build_base_image(config, runtime)?;
    }
    if let Some(dockerfile) = repo_overlay_dockerfile(workspace)? {
        let image_name = repo_overlay_image_name(workspace);
        build_repo_overlay_image(config, runtime, workspace, &dockerfile, &image_name)?;
        Ok(image_name)
    } else {
        Ok(config.image_name().to_string())
    }
}

fn build_copilot_image(config: &AppConfig, runtime: &str, workspace: &Path) -> Result<String> {
    build_base_image(config, runtime)?;
    if let Some(dockerfile) = repo_overlay_dockerfile(workspace)? {
        let image_name = repo_overlay_image_name(workspace);
        build_repo_overlay_image(config, runtime, workspace, &dockerfile, &image_name)?;
        Ok(image_name)
    } else {
        Ok(config.image_name().to_string())
    }
}

fn build_base_image(config: &AppConfig, runtime: &str) -> Result<()> {
    run_status(
        Command::new(runtime).args([
            "build",
            "-t",
            config.image_name(),
            config.repo_root().to_str().unwrap_or("."),
        ]),
        "failed to build container image",
    )
}

fn build_repo_overlay_image(
    config: &AppConfig,
    runtime: &str,
    workspace: &Path,
    dockerfile: &Path,
    image_name: &str,
) -> Result<()> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
    let mut command = Command::new(runtime);
    command
        .arg("build")
        .arg("-t")
        .arg(image_name)
        .arg("-f")
        .arg(dockerfile)
        .arg("--build-arg")
        .arg(format!(
            "{REPO_OVERLAY_BASE_IMAGE_ARG}={}",
            config.image_name()
        ))
        .arg(&workspace);
    run_status(
        &mut command,
        &format!(
            "failed to build repo overlay image from {}",
            dockerfile.display()
        ),
    )
}

fn repo_overlay_dockerfile(workspace: &Path) -> Result<Option<PathBuf>> {
    let path = workspace.join(REPO_OVERLAY_DOCKERFILE);
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_file() {
        bail!(
            "repo overlay path exists but is not a file: {}",
            path.display()
        );
    }
    Ok(Some(path))
}

fn repo_overlay_image_name(workspace: &Path) -> String {
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    format!("llm-box-workspace-{}", workspace_key(&workspace))
}

fn image_exists(runtime: &str, image_name: &str) -> Result<bool> {
    let status = Command::new(runtime)
        .args(["image", "inspect", image_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to inspect image `{image_name}`"))?;
    Ok(status.success())
}

fn image_supports_broker(runtime: &str, image_name: &str) -> Result<bool> {
    let output = Command::new(runtime)
        .args([
            "image",
            "inspect",
            "--format",
            "{{index .Config.Labels \"io.github.llm-box.egress-broker\"}}",
            image_name,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to inspect labels for image `{image_name}`"))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "1")
}

fn image_entrypoint_uses_copilot(runtime: &str, image_name: &str) -> Result<bool> {
    let output = Command::new(runtime)
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .Config.Entrypoint}}",
            image_name,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to inspect entrypoint for image `{image_name}`"))?;
    if !output.status.success() {
        return Ok(false);
    }
    let entrypoint =
        String::from_utf8(output.stdout).context("image entrypoint was not valid utf-8")?;
    Ok(entrypoint_invokes_copilot(&entrypoint))
}

fn entrypoint_invokes_copilot(entrypoint: &str) -> bool {
    entrypoint.to_ascii_lowercase().contains("copilot")
}

fn detect_runtime() -> Result<String> {
    if let Some(value) = env::var_os("LLM_BOX_RUNTIME") {
        return Ok(value.to_string_lossy().into_owned());
    }
    if command_exists("docker") {
        return Ok("docker".to_string());
    }
    if command_exists("podman") {
        return Ok("podman".to_string());
    }
    bail!("no supported container runtime found (expected docker or podman)");
}

fn ensure_runtime_ready(runtime: &str) -> Result<()> {
    let output = Command::new(runtime)
        .arg("info")
        .output()
        .with_context(|| format!("failed to run `{runtime} info`"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    if runtime == "podman"
        && cfg!(target_os = "macos")
        && combined.to_ascii_lowercase().contains("podman")
        && (combined.to_ascii_lowercase().contains("cannot connect")
            || combined
                .to_ascii_lowercase()
                .contains("unable to connect to podman socket")
            || combined.to_ascii_lowercase().contains("podman machine"))
    {
        bail!("Podman is installed but not running. Run `podman machine start` and retry.");
    }

    bail!("{}", combined.trim());
}

fn run_status(command: &mut Command, context: &str) -> Result<()> {
    let status = command.status().with_context(|| context.to_string())?;
    if !status.success() {
        bail!("{context}");
    }
    Ok(())
}

fn current_command_output<I, S>(program: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{program}`"))?;
    if !output.status.success() {
        bail!("`{program}` exited unsuccessfully");
    }
    Ok(String::from_utf8(output.stdout)
        .context("command output was not valid utf-8")?
        .trim()
        .to_string())
}

fn try_optional_command_output<I, S>(
    program: &str,
    args: I,
    env_overrides: &[(&str, &str)],
    timeout: Duration,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let stdout_path = unique_runtime_temp_path(program, "stdout");
    let stderr_path = unique_runtime_temp_path(program, "stderr");
    let stdout = File::create(&stdout_path)
        .with_context(|| format!("failed to create {}", stdout_path.display()))?;
    let stderr = File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env_overrides.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                return Ok(None);
            }
            Ok(None) => thread::sleep(OPTIONAL_COMMAND_POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                return Err(error).with_context(|| format!("failed to poll `{program}`"));
            }
        }
    };
    let stdout = fs::read(stdout_path.as_path())
        .with_context(|| format!("failed to read {}", stdout_path.display()))?;
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    if !status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(stdout).context("command output was not valid utf-8")?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn unique_runtime_temp_path(program: &str, suffix: &str) -> PathBuf {
    let sanitized = program.replace(std::path::MAIN_SEPARATOR, "_");
    env::temp_dir().join(format!(
        "llm-box-{sanitized}-{suffix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn workspace_home_for(config: &AppConfig, workspace: &Path) -> PathBuf {
    config
        .workspaces_root()
        .join(workspace_key(workspace))
        .join("home")
}

fn migrate_workspace_home_state(workspace_home: &Path) -> Result<()> {
    let state_dir = workspace_home.join(".llm-box");
    fs::create_dir_all(state_dir.join("providers/copilot")).with_context(|| {
        format!(
            "failed to create {}",
            state_dir.join("providers/copilot").display()
        )
    })?;
    let version_file = state_dir.join(WORKSPACE_STATE_VERSION_FILE);
    let version = read_workspace_state_version(&version_file)?;
    if version < 1 {
        migrate_workspace_home_v1(workspace_home)?;
    }
    if version != WORKSPACE_STATE_VERSION {
        write_atomic(
            &version_file,
            format!("{WORKSPACE_STATE_VERSION}\n").as_bytes(),
        )?;
    }
    Ok(())
}

fn read_workspace_state_version(path: &Path) -> Result<u32> {
    match fs::read_to_string(path) {
        Ok(raw) => raw
            .trim()
            .parse::<u32>()
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn migrate_workspace_home_v1(workspace_home: &Path) -> Result<()> {
    let copilot_dir = workspace_home.join(".copilot");
    remove_file_if_exists(&copilot_dir.join(LEGACY_AUTH_MARKER_FILE))?;
    remove_file_if_exists(
        &copilot_dir.join(format!(".{}.bootstrap.lock", LEGACY_AUTH_MARKER_FILE)),
    )?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn command_exists(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|path| path.join(program).is_file())
}

#[derive(Debug)]
struct SessionNetwork {
    runtime: String,
    internal_name: String,
    external_name: String,
}

impl SessionNetwork {
    fn create(runtime: &str, session_id: &str) -> Result<Self> {
        let network = Self {
            runtime: runtime.to_string(),
            internal_name: format!("llm-box-internal-{session_id}"),
            external_name: format!("llm-box-external-{session_id}"),
        };
        let mut internal_create = Command::new(runtime);
        internal_create
            .args(["network", "create", "--internal", &network.internal_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_status(
            &mut internal_create,
            "failed to create internal llm-box network",
        )?;
        let mut external_create = Command::new(runtime);
        external_create
            .args(["network", "create", &network.external_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Err(error) = run_status(
            &mut external_create,
            "failed to create external llm-box network",
        ) {
            network.remove_network(&network.internal_name);
            return Err(error);
        }
        Ok(network)
    }

    fn internal_name(&self) -> &str {
        &self.internal_name
    }

    fn external_name(&self) -> &str {
        &self.external_name
    }

    fn remove_network(&self, name: &str) {
        let _ = Command::new(&self.runtime)
            .args(["network", "rm", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for SessionNetwork {
    fn drop(&mut self) {
        self.remove_network(&self.internal_name);
        self.remove_network(&self.external_name);
    }
}

#[derive(Debug)]
struct BrokerProcess {
    runtime: String,
    container_name: String,
    child: Child,
    ready_file: PathBuf,
}

impl BrokerProcess {
    fn start(
        runtime: &str,
        image_name: &str,
        session: &SessionContext,
        network: &SessionNetwork,
    ) -> Result<Self> {
        let _ = fs::remove_file(session.broker_ready_file());

        let log_file = fs::File::create(session.broker_log_file())
            .with_context(|| format!("failed to create {}", session.broker_log_file().display()))?;
        let log_file_err = log_file
            .try_clone()
            .context("failed to clone broker log handle")?;
        let container_name = format!("llm-box-broker-{}", session.session_id());
        let loopback_alias = runtime_host_loopback_alias(runtime);
        let plan = build_broker_run_plan(
            runtime,
            image_name,
            session,
            network,
            &loopback_alias,
            cfg!(target_os = "linux"),
        );
        let mut command = plan.command(runtime);
        let mut child = command
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .spawn()
            .context("failed to start broker process")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut external_connected = false;
        loop {
            if !external_connected && container_is_running(runtime, &container_name)? {
                let mut connect_external = Command::new(runtime);
                connect_external
                    .args([
                        "network",
                        "connect",
                        network.external_name(),
                        &container_name,
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Err(error) = run_status(
                    &mut connect_external,
                    "failed to attach egress broker to external llm-box network",
                ) {
                    stop_container(runtime, &container_name);
                    let _ = child.wait();
                    return Err(error);
                }
                external_connected = true;
            }
            if let Ok(payload) = fs::read_to_string(session.broker_ready_file()) {
                if let Ok(info) = serde_json::from_str::<broker::BrokerReady>(&payload) {
                    if info.listen_port == BROKER_LISTEN_PORT {
                        return Ok(Self {
                            runtime: runtime.to_string(),
                            container_name,
                            child,
                            ready_file: session.broker_ready_file().to_path_buf(),
                        });
                    }
                }
            }

            if Instant::now() >= deadline {
                stop_container(runtime, &container_name);
                let _ = child.wait();
                bail!(
                    "egress broker failed to become ready; see {}",
                    session.broker_log_file().display()
                );
            }

            if child
                .try_wait()
                .context("failed to poll broker process")?
                .is_some()
            {
                bail!(
                    "egress broker exited before becoming ready; see {}",
                    session.broker_log_file().display()
                );
            }

            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for BrokerProcess {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ready_file);
        if let Ok(None) = self.child.try_wait() {
            stop_container(&self.runtime, &self.container_name);
            let _ = self.child.wait();
        }
    }
}

fn container_is_running(runtime: &str, container_name: &str) -> Result<bool> {
    let output = Command::new(runtime)
        .args(["inspect", "--format", "{{.State.Running}}", container_name])
        .output()
        .with_context(|| format!("failed to inspect container `{container_name}`"))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn runtime_host_loopback_alias(runtime: &str) -> String {
    if let Some(value) = env::var_os("LLM_BOX_HOST_LOOPBACK_ALIAS") {
        return value.to_string_lossy().into_owned();
    }
    if runtime == "podman" {
        "host.containers.internal".to_string()
    } else {
        "host.docker.internal".to_string()
    }
}

fn stop_container(runtime: &str, container_name: &str) {
    let _ = Command::new(runtime)
        .args(["stop", "--time", "1", container_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn repo_overlay_dockerfile_is_detected() {
        let root = unique_test_dir("repo-overlay-dockerfile");
        let workspace = root.join("workspace");
        let overlay = workspace.join(REPO_OVERLAY_DOCKERFILE);
        fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        fs::write(
            &overlay,
            "ARG LLM_BOX_BASE_IMAGE\nFROM ${LLM_BOX_BASE_IMAGE}\n",
        )
        .unwrap();

        assert_eq!(repo_overlay_dockerfile(&workspace).unwrap(), Some(overlay));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_overlay_image_name_is_workspace_specific() {
        let root = unique_test_dir("repo-overlay-image-name");
        let workspace_a = root.join("workspace-a");
        let workspace_b = root.join("workspace-b");
        fs::create_dir_all(&workspace_a).unwrap();
        fs::create_dir_all(&workspace_b).unwrap();

        let image_a = repo_overlay_image_name(&workspace_a);
        let image_b = repo_overlay_image_name(&workspace_b);

        assert!(image_a.starts_with("llm-box-workspace-"));
        assert!(image_b.starts_with("llm-box-workspace-"));
        assert_ne!(image_a, image_b);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_repo_overlay_dockerfile_writes_template() {
        let root = unique_test_dir("init-repo-overlay");
        fs::create_dir_all(&root).unwrap();

        let dockerfile = init_repo_overlay_dockerfile(&root).unwrap();

        assert_eq!(dockerfile, root.join(REPO_OVERLAY_DOCKERFILE));
        assert_eq!(
            fs::read_to_string(&dockerfile).unwrap(),
            INIT_IMAGE_TEMPLATE
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_repo_overlay_dockerfile_does_not_overwrite_existing_file() {
        let root = unique_test_dir("init-repo-overlay-existing");
        let dockerfile = root.join(REPO_OVERLAY_DOCKERFILE);
        fs::create_dir_all(dockerfile.parent().unwrap()).unwrap();
        fs::write(&dockerfile, "existing\n").unwrap();

        let error = init_repo_overlay_dockerfile(&root).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("repo overlay Dockerfile already exists")
        );
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), "existing\n");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copilot_run_plan_captures_security_relevant_runtime_args() {
        let root = unique_test_dir("copilot-run-plan");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        let shared_skills = root.join("shared-skills");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&shared_skills).unwrap();
        let session =
            SessionContext::new_session(&config, workspace.clone(), &["--resume".to_string()])
                .unwrap();
        let network = SessionNetwork {
            runtime: "podman".to_string(),
            internal_name: "llm-box-internal-test".to_string(),
            external_name: "llm-box-external-test".to_string(),
        };
        let invocation = vec![
            "--config-dir".to_string(),
            MANAGED_COPILOT_CONFIG_DIR.to_string(),
            "--resume".to_string(),
            "session-123".to_string(),
        ];
        let passthrough_env = vec![
            (
                "COPILOT_GITHUB_TOKEN".to_string(),
                "copilot-token".to_string(),
            ),
            ("GH_TOKEN".to_string(), "gh-token".to_string()),
            ("GITHUB_TOKEN".to_string(), "github-token".to_string()),
        ];
        let secret_env_vars = vec![
            "COPILOT_GITHUB_TOKEN".to_string(),
            "GH_TOKEN".to_string(),
            "GITHUB_TOKEN".to_string(),
        ];

        let plan = build_copilot_run_plan(
            "test-image:latest",
            &session,
            &network,
            "501",
            "20",
            TtyMode::Interactive,
            &invocation,
            &passthrough_env,
            &secret_env_vars,
            Some(shared_skills.as_path()),
        );

        assert_eq!(
            plan.args,
            vec![
                "run".to_string(),
                "-it".to_string(),
                "--rm".to_string(),
                "--user".to_string(),
                "501:20".to_string(),
                "--workdir".to_string(),
                "/workspace".to_string(),
                "--network".to_string(),
                "llm-box-internal-test".to_string(),
                "--cap-drop=ALL".to_string(),
                "--security-opt=no-new-privileges".to_string(),
                "-e".to_string(),
                "HOME=/home/copilot".to_string(),
                "-e".to_string(),
                format!("LLM_BOX_SESSION_ID={}", session.session_id()),
                "-e".to_string(),
                "COPILOT_GITHUB_TOKEN=copilot-token".to_string(),
                "-e".to_string(),
                "GH_TOKEN=gh-token".to_string(),
                "-e".to_string(),
                "GITHUB_TOKEN=github-token".to_string(),
                "-e".to_string(),
                format!("HTTP_PROXY=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("HTTPS_PROXY=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("ALL_PROXY=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("http_proxy=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("https_proxy=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("all_proxy=http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}"),
                "-e".to_string(),
                format!("NO_PROXY=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"),
                "-e".to_string(),
                format!("no_proxy=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"),
                "-v".to_string(),
                format!("{}:/home/copilot", session.workspace_home().display()),
                "-v".to_string(),
                format!(
                    "{}:/home/copilot/.copilot/skills:ro",
                    shared_skills.display()
                ),
                "-v".to_string(),
                format!("{}:/workspace", session.workspace().display()),
                "test-image:latest".to_string(),
                "--secret-env-vars=COPILOT_GITHUB_TOKEN,GH_TOKEN,GITHUB_TOKEN".to_string(),
                "--config-dir".to_string(),
                MANAGED_COPILOT_CONFIG_DIR.to_string(),
                "--resume".to_string(),
                "session-123".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_auth_env_prefers_explicit_tokens() {
        let resolved = resolve_provider_auth_env(
            vec![
                ("GH_TOKEN".to_string(), "gh-token".to_string()),
                ("GITHUB_TOKEN".to_string(), "github-token".to_string()),
            ],
            GhAuthStatus::Authenticated("host-token".to_string()),
        );

        assert_eq!(
            resolved,
            ProviderAuthEnv {
                passthrough_env: vec![
                    ("GH_TOKEN".to_string(), "gh-token".to_string()),
                    ("GITHUB_TOKEN".to_string(), "github-token".to_string()),
                ],
                secret_env_vars: vec!["GH_TOKEN".to_string(), "GITHUB_TOKEN".to_string()],
                user_tip: None,
            }
        );
    }

    #[test]
    fn provider_auth_env_uses_host_gh_token_when_needed() {
        let resolved = resolve_provider_auth_env(
            Vec::new(),
            GhAuthStatus::Authenticated("host-token".to_string()),
        );

        assert_eq!(
            resolved,
            ProviderAuthEnv {
                passthrough_env: vec![(
                    "COPILOT_GITHUB_TOKEN".to_string(),
                    "host-token".to_string(),
                )],
                secret_env_vars: vec!["COPILOT_GITHUB_TOKEN".to_string()],
                user_tip: None,
            }
        );
    }

    #[test]
    fn provider_auth_env_emits_tip_when_gh_needs_login() {
        let resolved = resolve_provider_auth_env(Vec::new(), GhAuthStatus::NotAuthenticated);

        assert_eq!(
            resolved,
            ProviderAuthEnv {
                passthrough_env: Vec::new(),
                secret_env_vars: Vec::new(),
                user_tip: Some(GH_AUTH_REUSE_TIP),
            }
        );
    }

    #[test]
    fn try_optional_command_output_times_out() {
        let result =
            try_optional_command_output("sh", ["-c", "sleep 1"], &[], Duration::from_millis(10))
                .unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn inject_managed_config_dir_only_when_missing() {
        assert_eq!(
            inject_managed_config_dir(&["--resume".to_string(), "abc".to_string()]),
            vec![
                "--config-dir".to_string(),
                MANAGED_COPILOT_CONFIG_DIR.to_string(),
                "--resume".to_string(),
                "abc".to_string(),
            ]
        );
        assert_eq!(
            inject_managed_config_dir(&[
                "--config-dir".to_string(),
                "/workspace/custom".to_string(),
                "--resume".to_string(),
            ]),
            vec![
                "--config-dir".to_string(),
                "/workspace/custom".to_string(),
                "--resume".to_string(),
            ]
        );
    }

    #[test]
    fn migrate_workspace_home_state_removes_only_legacy_llm_box_artifacts() {
        let root = unique_test_dir("migrate-workspace-home");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        let workspace_home = workspace_home_for(&config, &workspace);
        let copilot_dir = workspace_home.join(".copilot");
        fs::create_dir_all(&copilot_dir).unwrap();
        fs::write(
            copilot_dir.join("config.json"),
            r#"{
  "banner": "never",
  "copilotTokens": { "https://github.com:user": "token" },
  "lastLoggedInUser": { "host": "https://github.com", "login": "user" },
  "loggedInUsers": [{ "host": "https://github.com", "login": "user" }]
}
"#,
        )
        .unwrap();
        fs::write(copilot_dir.join(LEGACY_AUTH_MARKER_FILE), "1\n").unwrap();
        fs::write(
            copilot_dir.join(format!(".{}.bootstrap.lock", LEGACY_AUTH_MARKER_FILE)),
            "lock\n",
        )
        .unwrap();

        migrate_workspace_home_state(&workspace_home).unwrap();

        let config_contents = fs::read_to_string(copilot_dir.join("config.json")).unwrap();
        assert!(config_contents.contains("copilotTokens"));
        assert!(config_contents.contains("lastLoggedInUser"));
        assert!(config_contents.contains("loggedInUsers"));
        assert!(!copilot_dir.join(LEGACY_AUTH_MARKER_FILE).exists());
        assert!(
            !copilot_dir
                .join(format!(".{}.bootstrap.lock", LEGACY_AUTH_MARKER_FILE))
                .exists()
        );
        assert_eq!(
            fs::read_to_string(
                workspace_home
                    .join(".llm-box")
                    .join(WORKSPACE_STATE_VERSION_FILE)
            )
            .unwrap(),
            format!("{WORKSPACE_STATE_VERSION}\n")
        );
        assert!(workspace_home.join(".llm-box/providers/copilot").is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migrate_workspace_home_state_is_idempotent() {
        let root = unique_test_dir("migrate-workspace-home-idempotent");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        let workspace_home = workspace_home_for(&config, &workspace);
        let copilot_dir = workspace_home.join(".copilot");
        fs::create_dir_all(&copilot_dir).unwrap();
        fs::write(
            copilot_dir.join("config.json"),
            r#"{
  "banner": "never",
  "copilotTokens": { "https://github.com:user": "token" }
}
"#,
        )
        .unwrap();
        fs::create_dir_all(workspace_home.join(".llm-box")).unwrap();
        fs::write(
            workspace_home
                .join(".llm-box")
                .join(WORKSPACE_STATE_VERSION_FILE),
            format!("{WORKSPACE_STATE_VERSION}\n"),
        )
        .unwrap();

        migrate_workspace_home_state(&workspace_home).unwrap();

        assert!(
            fs::read_to_string(copilot_dir.join("config.json"))
                .unwrap()
                .contains("copilotTokens")
        );
        assert_eq!(
            fs::read_to_string(
                workspace_home
                    .join(".llm-box")
                    .join(WORKSPACE_STATE_VERSION_FILE)
            )
            .unwrap(),
            format!("{WORKSPACE_STATE_VERSION}\n")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gh_auth_tip_only_shows_for_real_session_launches() {
        assert!(should_emit_gh_auth_tip(&[]));
        assert!(should_emit_gh_auth_tip(&[
            "--resume".to_string(),
            "session-123".to_string()
        ]));
        assert!(!should_emit_gh_auth_tip(&["login".to_string()]));
        assert!(!should_emit_gh_auth_tip(&["--help".to_string()]));
        assert!(!should_emit_gh_auth_tip(&["version".to_string()]));
    }

    #[test]
    fn entrypoint_invokes_copilot_detects_wrapper_scripts() {
        assert!(entrypoint_invokes_copilot(r#"["copilot"]"#));
        assert!(entrypoint_invokes_copilot(
            r#"["sh","-lc","echo overlay-entrypoint; exec copilot \"$@\"","--"]"#
        ));
        assert!(!entrypoint_invokes_copilot(r#"["bash","-lc"]"#));
    }

    #[test]
    fn broker_run_plan_captures_session_mounts_and_broker_files() {
        let root = unique_test_dir("broker-run-plan");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();
        let network = SessionNetwork {
            runtime: "docker".to_string(),
            internal_name: "llm-box-internal-test".to_string(),
            external_name: "llm-box-external-test".to_string(),
        };

        let plan = build_broker_run_plan(
            "docker",
            "test-image:latest",
            &session,
            &network,
            "host.docker.internal",
            true,
        );

        assert_eq!(
            plan.args,
            vec![
                "run".to_string(),
                "--rm".to_string(),
                "--name".to_string(),
                format!("llm-box-broker-{}", session.session_id()),
                "--network".to_string(),
                "llm-box-internal-test".to_string(),
                "--network-alias".to_string(),
                BROKER_INTERNAL_HOST.to_string(),
                "-v".to_string(),
                format!("{}:/llm-box/session", session.session_dir().display()),
                "--entrypoint".to_string(),
                "llm-box".to_string(),
                "--add-host=host.docker.internal:host-gateway".to_string(),
                "test-image:latest".to_string(),
                "__broker".to_string(),
                "--listen-host".to_string(),
                "0.0.0.0".to_string(),
                "--listen-port".to_string(),
                BROKER_LISTEN_PORT.to_string(),
                "--allowed-targets-file".to_string(),
                "/llm-box/session/allowed-targets.txt".to_string(),
                "--pending-events-file".to_string(),
                "/llm-box/session/pending-events.jsonl".to_string(),
                "--connectors-file".to_string(),
                "/llm-box/session/connectors.json".to_string(),
                "--broker-ready-file".to_string(),
                "/llm-box/session/broker-ready.json".to_string(),
                "--host-loopback-alias".to_string(),
                "host.docker.internal".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(root);
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

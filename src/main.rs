mod broker;
mod config;
mod egress;
mod session;
mod test_support;
mod webui;

use crate::config::AppConfig;
use crate::egress::{BROKER_INTERNAL_HOST, parse_target_spec};
use crate::session::{SessionContext, workspace_key, write_atomic};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const REPO_OVERLAY_DOCKERFILE: &str = ".llm-box/Dockerfile";
const REPO_OVERLAY_BASE_IMAGE_ARG: &str = "LLM_BOX_BASE_IMAGE";
const BROKER_LISTEN_PORT: u16 = 3128;
const INIT_IMAGE_TEMPLATE: &str = r#"ARG LLM_BOX_BASE_IMAGE
FROM ${LLM_BOX_BASE_IMAGE}

USER root
# RUN apt-get update \
#   && apt-get install -y --no-install-recommends gh \
#   && rm -rf /var/lib/apt/lists/*
USER copilot
"#;

#[derive(Parser, Debug)]
#[command(
    name = "llm-box",
    version,
    about = "Boxed LLM CLI runner with live network approvals"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Build,
    InitImage,
    Defaults {
        #[command(subcommand)]
        command: DefaultCommands,
    },
    Pending,
    Allowed,
    Allow {
        target: String,
    },
    Deny {
        target: String,
    },
    Dismiss {
        target: String,
    },
    Endpoint {
        target: String,
    },
    Ui(UiArgs),
    Copilot(CopilotArgs),
    #[command(hide = true, name = "__broker")]
    Broker(BrokerCommandArgs),
    #[command(hide = true, name = "__session-ui")]
    SessionUi(SessionUiArgs),
    #[command(hide = true, name = "__test-free-port")]
    TestFreePort,
    #[command(hide = true, name = "__test-latest-session-dir")]
    TestLatestSessionDir(TestLatestSessionDirArgs),
    #[command(hide = true, name = "__test-workspace-home")]
    TestWorkspaceHome(TestLatestSessionDirArgs),
    #[command(hide = true, name = "__serve-static")]
    ServeStatic(ServeStaticArgs),
}

#[derive(Subcommand, Debug)]
enum DefaultCommands {
    List,
    Add { target: String },
    Remove { target: String },
}

#[derive(Args, Debug)]
#[command(trailing_var_arg = true)]
struct CopilotArgs {
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct UiArgs {
    #[arg(long)]
    session: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct BrokerCommandArgs {
    #[arg(long)]
    pub(crate) listen_host: String,
    #[arg(long)]
    pub(crate) listen_port: u16,
    #[arg(long = "allowed-targets-file")]
    pub(crate) allowed_targets_file: PathBuf,
    #[arg(long = "pending-events-file")]
    pub(crate) pending_events_file: PathBuf,
    #[arg(long)]
    pub(crate) connectors_file: PathBuf,
    #[arg(long = "broker-ready-file")]
    pub(crate) broker_ready_file: PathBuf,
    #[arg(long)]
    pub(crate) host_loopback_alias: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SessionUiArgs {
    #[arg(long)]
    pub(crate) listen_host: String,
    #[arg(long)]
    pub(crate) listen_port: u16,
    #[arg(long)]
    pub(crate) session_dir: PathBuf,
    #[arg(long)]
    pub(crate) ready_file: PathBuf,
}

#[derive(Args, Debug)]
pub(crate) struct TestLatestSessionDirArgs {
    pub(crate) workspace: PathBuf,
    pub(crate) root: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ServeStaticArgs {
    #[arg(long)]
    pub(crate) listen_host: String,
    #[arg(long)]
    pub(crate) listen_port: u16,
    #[arg(long)]
    pub(crate) directory: PathBuf,
}

fn main() {
    let exit_code = match try_main() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("llm-box: {error:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn try_main() -> Result<i32> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build => {
            let config = AppConfig::detect()?;
            build_image(&config)?;
            Ok(0)
        }
        Commands::InitImage => {
            let workspace = env::current_dir().context("failed to resolve current directory")?;
            let dockerfile = init_repo_overlay_dockerfile(&workspace)?;
            println!("{}", dockerfile.display());
            Ok(0)
        }
        Commands::Defaults { command } => match command {
            DefaultCommands::List => {
                let config = AppConfig::detect()?;
                for target in config.user_default_targets()? {
                    println!("{target}");
                }
                Ok(0)
            }
            DefaultCommands::Add { target } => {
                let config = AppConfig::detect()?;
                let normalized = parse_target_spec(&target)?;
                config.add_user_default_target(&target)?;
                println!("default added {normalized}");
                Ok(0)
            }
            DefaultCommands::Remove { target } => {
                let config = AppConfig::detect()?;
                let normalized = parse_target_spec(&target)?;
                config.remove_user_default_target(&target)?;
                println!("default removed {normalized}");
                Ok(0)
            }
        },
        Commands::Pending => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, None)?;
            for item in session.pending_items()? {
                if let Some(endpoint) = item.connector_endpoint {
                    println!(
                        "{}\t{}\t{}",
                        item.target, endpoint, item.last_seen_epoch_nanos
                    );
                } else {
                    println!("{}\t{}", item.target, item.last_seen_epoch_nanos);
                }
            }
            Ok(0)
        }
        Commands::Allowed => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, None)?;
            for target in session.allowed_items()? {
                if let Some(endpoint) = target.connector_endpoint {
                    println!("{}\t{}", target.target, endpoint);
                } else {
                    println!("{}", target.target);
                }
            }
            Ok(0)
        }
        Commands::Allow { target } => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, None)?;
            let normalized = parse_target_spec(&target)?;
            session.allow_target(&target)?;
            if normalized.uses_connector() {
                let connector = session.connector_endpoint(&target)?;
                println!("allowed {}\t{}", normalized, connector.endpoint());
            } else {
                println!("allowed {normalized}");
            }
            Ok(0)
        }
        Commands::Deny { target } => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, None)?;
            let normalized = parse_target_spec(&target)?;
            session.deny_target(&target)?;
            println!("denied {normalized}");
            Ok(0)
        }
        Commands::Dismiss { target } => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, None)?;
            let normalized = parse_target_spec(&target)?;
            session.dismiss_target(&target)?;
            println!("dismissed {normalized}");
            Ok(0)
        }
        Commands::Endpoint { target } => {
            match AppConfig::detect()
                .and_then(|config| resolve_session_for_current_workspace(&config, None))
            {
                Ok(session) => {
                    let connector = session.connector_endpoint(&target)?;
                    println!("{}\t{}", connector.target, connector.endpoint());
                }
                Err(_) => {
                    let (target, endpoint) = request_connector_endpoint_from_broker(&target)?;
                    println!("{target}\t{endpoint}");
                }
            }
            Ok(0)
        }
        Commands::Ui(args) => {
            let config = AppConfig::detect()?;
            let session = resolve_session_for_current_workspace(&config, args.session)?;
            let mut ui = BrowserUiProcess::start(&session)?;
            println!("{}", ui.url);
            open_browser(&ui.url)
                .with_context(|| format!("failed to open browser companion at {}", ui.url))?;
            let _ = ui.child.wait();
            Ok(0)
        }
        Commands::Copilot(args) => {
            let config = AppConfig::detect()?;
            run_copilot(&config, &args.args)
        }
        Commands::Broker(args) => broker::run_broker_command(args),
        Commands::SessionUi(args) => webui::run_session_ui_command(args),
        Commands::TestFreePort => {
            println!("{}", test_support::find_free_port()?);
            Ok(0)
        }
        Commands::TestLatestSessionDir(args) => {
            println!(
                "{}",
                test_support::latest_session_dir(&args.workspace, &args.root)?.display()
            );
            Ok(0)
        }
        Commands::TestWorkspaceHome(args) => {
            println!(
                "{}",
                test_support::workspace_home_dir(&args.workspace, &args.root).display()
            );
            Ok(0)
        }
        Commands::ServeStatic(args) => test_support::serve_static_command(args),
    }
}

fn run_copilot(config: &AppConfig, args: &[String]) -> Result<i32> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    let image_name = ensure_copilot_image(config, &runtime, &workspace)?;
    let session = SessionContext::new_session(config, workspace, args)?;
    let network = SessionNetwork::create(&runtime, session.session_id())?;
    let broker = BrokerProcess::start(&runtime, &image_name, &session, &network)?;
    let ui = BrowserUiProcess::maybe_start(&session)?;

    let tty_args = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        vec!["-it".to_string()]
    } else {
        vec!["-i".to_string()]
    };

    let mut command = Command::new(&runtime);
    command.arg("run");
    for arg in tty_args {
        command.arg(arg);
    }
    command.arg("--rm");

    let uid = current_command_output("id", ["-u"])?;
    let gid = current_command_output("id", ["-g"])?;

    command.args([
        "--user",
        &format!("{uid}:{gid}"),
        "--workdir",
        "/workspace",
        "--network",
        network.internal_name(),
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "-e",
        "HOME=/home/copilot",
        "-e",
        &format!("LLM_BOX_SESSION_ID={}", session.session_id()),
    ]);

    pass_env(&mut command, "GH_TOKEN");
    pass_env(&mut command, "GITHUB_TOKEN");
    let proxy_url = format!("http://{BROKER_INTERNAL_HOST}:{BROKER_LISTEN_PORT}");
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.arg("-e").arg(format!("{key}={proxy_url}"));
    }
    command.arg("-e").arg(format!(
        "NO_PROXY=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"
    ));
    command.arg("-e").arg(format!(
        "no_proxy=localhost,127.0.0.1,{BROKER_INTERNAL_HOST}"
    ));
    command.arg("-v").arg(format!(
        "{}:/home/copilot",
        session.workspace_home().display()
    ));
    if let Some(shared_skills_dir) = config.shared_copilot_skills_dir() {
        command.arg("-v").arg(format!(
            "{}:/home/copilot/.copilot/skills:ro",
            shared_skills_dir.display()
        ));
    }
    command
        .arg("-v")
        .arg(format!("{}:/workspace", session.workspace().display()));
    command.arg(&image_name);
    for arg in args {
        command.arg(arg);
    }

    let status = command
        .status()
        .context("failed to launch container runtime")?;
    drop(ui);
    drop(broker);
    drop(network);
    Ok(status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 }))
}

fn build_image(config: &AppConfig) -> Result<()> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    build_copilot_image(config, &runtime, &workspace).map(|_| ())
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

fn resolve_session_for_current_workspace(
    config: &AppConfig,
    session_id: Option<String>,
) -> Result<SessionContext> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    match session_id {
        Some(id) => SessionContext::from_session_id(config, workspace, id),
        None => SessionContext::latest_for_workspace(config, workspace),
    }
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

fn pass_env(command: &mut Command, key: &str) {
    if let Some(value) = env::var_os(key) {
        command
            .arg("-e")
            .arg(format!("{key}={}", value.to_string_lossy()));
    }
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

fn command_exists(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|path| path.join(program).is_file())
}

fn init_repo_overlay_dockerfile(workspace: &Path) -> Result<PathBuf> {
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
        let session_mount = format!("{}:/llm-box/session", session.session_dir().display());
        let loopback_alias = runtime_host_loopback_alias(runtime);
        let broker_port = BROKER_LISTEN_PORT.to_string();

        let mut command = Command::new(runtime);
        command
            .arg("run")
            .arg("--rm")
            .arg("--name")
            .arg(&container_name)
            .arg("--network")
            .arg(network.internal_name())
            .arg("--network-alias")
            .arg(BROKER_INTERNAL_HOST)
            .arg("-v")
            .arg(session_mount)
            .arg("--entrypoint")
            .arg("llm-box");
        if runtime == "docker" && cfg!(target_os = "linux") {
            command.arg("--add-host=host.docker.internal:host-gateway");
        }
        command.arg(image_name).arg("__broker").args([
            "--listen-host",
            "0.0.0.0",
            "--listen-port",
            &broker_port,
            "--allowed-targets-file",
            "/llm-box/session/allowed-targets.txt",
            "--pending-events-file",
            "/llm-box/session/pending-events.jsonl",
            "--connectors-file",
            "/llm-box/session/connectors.json",
            "--broker-ready-file",
            "/llm-box/session/broker-ready.json",
            "--host-loopback-alias",
            &loopback_alias,
        ]);
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

fn request_connector_endpoint_from_broker(target: &str) -> Result<(String, String)> {
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

#[derive(Debug)]
struct BrowserUiProcess {
    child: Child,
    ready_file: PathBuf,
    url: String,
}

impl BrowserUiProcess {
    fn maybe_start(session: &SessionContext) -> Result<Option<Self>> {
        if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
            return Ok(None);
        }
        if env::var_os("LLM_BOX_NO_BROWSER").is_some() {
            return Ok(None);
        }
        let ui = Self::start(session)?;
        eprintln!("llm-box browser companion: {}", ui.url);
        if let Err(error) = open_browser(&ui.url) {
            eprintln!(
                "llm-box: failed to open browser automatically; open {} manually ({error:#})",
                ui.url
            );
        }
        Ok(Some(ui))
    }

    fn start(session: &SessionContext) -> Result<Self> {
        let _ = fs::remove_file(session.ui_ready_file());
        let executable = env::current_exe().context("failed to resolve current executable")?;
        let mut child = Command::new(executable)
            .arg("__session-ui")
            .args([
                "--listen-host",
                "127.0.0.1",
                "--listen-port",
                "0",
                "--session-dir",
            ])
            .arg(session.session_dir())
            .arg("--ready-file")
            .arg(session.ui_ready_file())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start browser companion")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(payload) = fs::read_to_string(session.ui_ready_file()) {
                if let Ok(info) = serde_json::from_str::<webui::UiReady>(&payload) {
                    let url = format!("http://127.0.0.1:{}/", info.listen_port);
                    return Ok(Self {
                        child,
                        ready_file: session.ui_ready_file().to_path_buf(),
                        url,
                    });
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!("browser companion failed to become ready");
            }
            if child
                .try_wait()
                .context("failed to poll browser companion")?
                .is_some()
            {
                bail!("browser companion exited before becoming ready");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for BrowserUiProcess {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ready_file);
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn open_browser(url: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn copilot_passthrough_accepts_hyphenated_args() {
        let cli = Cli::try_parse_from(["llm-box", "copilot", "--resume", "session-123"]).unwrap();
        match cli.command {
            Commands::Copilot(provider) => {
                assert_eq!(provider.args, vec!["--resume", "session-123"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

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

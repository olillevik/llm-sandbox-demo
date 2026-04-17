mod proxy;
mod test_support;
mod webui;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ALLOWED_HOSTS: &[&str] = &["api.github.com", "api.business.githubcopilot.com"];

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
    Defaults {
        #[command(subcommand)]
        command: DefaultCommands,
    },
    Pending,
    Allowed,
    Allow {
        host: String,
    },
    Deny {
        host: String,
    },
    Dismiss {
        host: String,
    },
    Ui(UiArgs),
    Copilot(ProviderArgs),
    #[command(hide = true, name = "__run-provider")]
    RunProvider(InternalRunProviderArgs),
    #[command(hide = true, name = "__proxy")]
    Proxy(ProxyCommandArgs),
    #[command(hide = true, name = "__session-ui")]
    SessionUi(SessionUiArgs),
    #[command(hide = true, name = "__test-free-port")]
    TestFreePort,
    #[command(hide = true, name = "__test-latest-session-dir")]
    TestLatestSessionDir(TestLatestSessionDirArgs),
    #[command(hide = true, name = "__serve-static")]
    ServeStatic(ServeStaticArgs),
}

#[derive(Subcommand, Debug)]
enum DefaultCommands {
    List,
    Add { host: String },
    Remove { host: String },
}

#[derive(Args, Debug)]
#[command(trailing_var_arg = true)]
struct ProviderArgs {
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
#[command(trailing_var_arg = true)]
struct InternalRunProviderArgs {
    provider: ProviderKind,
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug, Default)]
struct UiArgs {
    #[arg(long)]
    session: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ProxyCommandArgs {
    #[arg(long)]
    pub(crate) listen_host: String,
    #[arg(long)]
    pub(crate) listen_port: u16,
    #[arg(long)]
    pub(crate) allowed_hosts_file: PathBuf,
    #[arg(long)]
    pub(crate) pending_log_file: PathBuf,
    #[arg(long)]
    pub(crate) workspace: String,
    #[arg(long)]
    pub(crate) ready_file: PathBuf,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderKind {
    Copilot,
}

impl ProviderKind {
    fn command_name(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
        }
    }
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
    let config = AppConfig::detect()?;

    match cli.command {
        Commands::Build => {
            build_image(&config)?;
            Ok(0)
        }
        Commands::Defaults { command } => match command {
            DefaultCommands::List => {
                for host in config.user_default_hosts()? {
                    println!("{host}");
                }
                Ok(0)
            }
            DefaultCommands::Add { host } => {
                config.add_user_default_host(&host)?;
                println!("default added {}", normalize_host(&host)?);
                Ok(0)
            }
            DefaultCommands::Remove { host } => {
                config.remove_user_default_host(&host)?;
                println!("default removed {}", normalize_host(&host)?);
                Ok(0)
            }
        },
        Commands::Pending => {
            let session = resolve_session_for_current_workspace(&config, None)?;
            for item in session.pending_items()? {
                if let Some(port) = item.port {
                    println!("{}:{}\t{}", item.host, port, item.timestamp);
                } else {
                    println!("{}\t{}", item.host, item.timestamp);
                }
            }
            Ok(0)
        }
        Commands::Allowed => {
            let session = resolve_session_for_current_workspace(&config, None)?;
            for host in session.allowed_hosts()? {
                println!("{host}");
            }
            Ok(0)
        }
        Commands::Allow { host } => {
            let session = resolve_session_for_current_workspace(&config, None)?;
            session.allow_host(&host)?;
            println!("allowed {}", normalize_host(&host)?);
            Ok(0)
        }
        Commands::Deny { host } => {
            let session = resolve_session_for_current_workspace(&config, None)?;
            session.deny_host(&host)?;
            println!("denied {}", normalize_host(&host)?);
            Ok(0)
        }
        Commands::Dismiss { host } => {
            let session = resolve_session_for_current_workspace(&config, None)?;
            session.dismiss_host(&host)?;
            println!("dismissed {}", normalize_host(&host)?);
            Ok(0)
        }
        Commands::Ui(args) => {
            let session = resolve_session_for_current_workspace(&config, args.session)?;
            let mut ui = BrowserUiProcess::start(&session)?;
            println!("{}", ui.url);
            let _ = ui.child.wait();
            Ok(0)
        }
        Commands::Copilot(args) => run_provider_entry(&config, ProviderKind::Copilot, args.args),
        Commands::RunProvider(args) => run_provider_direct(&config, args.provider, &args.args),
        Commands::Proxy(args) => proxy::run_proxy_command(args),
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
        Commands::ServeStatic(args) => test_support::serve_static_command(args),
    }
}

fn run_provider_entry(
    config: &AppConfig,
    provider: ProviderKind,
    args: Vec<String>,
) -> Result<i32> {
    run_provider_direct(config, provider, &args)
}

fn run_provider_direct(config: &AppConfig, provider: ProviderKind, args: &[String]) -> Result<i32> {
    let workspace = env::current_dir().context("failed to resolve current directory")?;
    let session = SessionContext::new_session(config, workspace, provider, args)?;
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    let proxy_host = proxy_host_for_runtime(&runtime);
    let proxy = ProxyProcess::start(&session)?;
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
    if runtime == "docker" && cfg!(target_os = "linux") {
        command.arg("--add-host=host.docker.internal:host-gateway");
    }

    let uid = current_command_output("id", ["-u"])?;
    let gid = current_command_output("id", ["-g"])?;

    command.args([
        "--user",
        &format!("{uid}:{gid}"),
        "--workdir",
        "/workspace",
        "--network=bridge",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "-e",
        "HOME=/home/copilot",
        "-e",
        &format!("LLM_BOX_SESSION_ID={}", session.session_id),
    ]);

    pass_env(&mut command, "GH_TOKEN");
    pass_env(&mut command, "GITHUB_TOKEN");
    let proxy_url = format!("http://{proxy_host}:{}", proxy.port);
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
    command.arg("-e").arg("NO_PROXY=localhost,127.0.0.1");
    command.arg("-e").arg("no_proxy=localhost,127.0.0.1");
    command.arg("-v").arg(format!(
        "{}:/home/copilot",
        session.container_home.display()
    ));
    command
        .arg("-v")
        .arg(format!("{}:/workspace", session.workspace.display()));
    command.arg(&config.image_name);
    for arg in args {
        command.arg(arg);
    }

    let status = command
        .status()
        .context("failed to launch container runtime")?;
    drop(ui);
    drop(proxy);
    Ok(status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 }))
}

fn build_image(config: &AppConfig) -> Result<()> {
    let runtime = detect_runtime()?;
    ensure_runtime_ready(&runtime)?;
    run_status(
        Command::new(runtime).args([
            "build",
            "-t",
            &config.image_name,
            config.repo_root.to_str().unwrap_or("."),
        ]),
        "failed to build container image",
    )
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

fn proxy_host_for_runtime(runtime: &str) -> String {
    if let Some(value) = env::var_os("LLM_BOX_PROXY_HOST") {
        return value.to_string_lossy().into_owned();
    }
    if runtime == "podman" {
        "host.containers.internal".to_string()
    } else {
        "host.docker.internal".to_string()
    }
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

#[derive(Debug)]
struct AppConfig {
    repo_root: PathBuf,
    config_root: PathBuf,
    workspaces_root: PathBuf,
    sessions_root: PathBuf,
    container_home: PathBuf,
    user_defaults_file: PathBuf,
    image_name: String,
}

impl AppConfig {
    fn detect() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let config_root = env::var_os("LLM_BOX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".llm-box"));
        let repo_root = detect_repo_root()?;
        Ok(Self {
            config_root: config_root.clone(),
            workspaces_root: config_root.join("workspaces"),
            sessions_root: config_root.join("sessions"),
            container_home: config_root.join("container-home"),
            user_defaults_file: config_root.join("default-allowed-hosts.txt"),
            image_name: env::var("LLM_BOX_IMAGE").unwrap_or_else(|_| "llm-box".to_string()),
            repo_root,
        })
    }

    fn ensure_config_root(&self) -> Result<()> {
        fs::create_dir_all(&self.config_root)
            .with_context(|| format!("failed to create {}", self.config_root.display()))
    }

    fn user_default_hosts(&self) -> Result<Vec<String>> {
        if !self.user_defaults_file.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.user_defaults_file)
            .with_context(|| format!("failed to read {}", self.user_defaults_file.display()))?;
        let mut hosts = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(normalize_host)
            .collect::<Result<Vec<_>>>()?;
        hosts.sort();
        hosts.dedup();
        Ok(hosts)
    }

    fn add_user_default_host(&self, host: &str) -> Result<()> {
        self.ensure_config_root()?;
        let mut hosts = self
            .user_default_hosts()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        hosts.insert(normalize_host(host)?);
        let contents = hosts
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.user_defaults_file, contents.as_bytes())
    }

    fn remove_user_default_host(&self, host: &str) -> Result<()> {
        self.ensure_config_root()?;
        let mut hosts = self
            .user_default_hosts()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        hosts.remove(&normalize_host(host)?);
        let contents = hosts
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.user_defaults_file, contents.as_bytes())
    }
}

fn detect_repo_root() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe() {
        candidates.extend(executable.ancestors().map(Path::to_path_buf));
    }
    if let Ok(current_dir) = env::current_dir() {
        candidates.extend(current_dir.ancestors().map(Path::to_path_buf));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for candidate in candidates {
        if candidate.join("Dockerfile").is_file() && candidate.join("Cargo.toml").is_file() {
            return Ok(candidate);
        }
    }

    bail!("failed to locate repo root containing Dockerfile and Cargo.toml");
}

#[derive(Debug, Clone)]
struct SessionContext {
    session_id: String,
    workspace: PathBuf,
    workspace_dir: PathBuf,
    session_dir: PathBuf,
    container_home: PathBuf,
    allowed_hosts_file: PathBuf,
    pending_log_file: PathBuf,
    dismissed_file: PathBuf,
    proxy_log_file: PathBuf,
    proxy_ready_file: PathBuf,
    ui_ready_file: PathBuf,
    session_meta_file: PathBuf,
}

impl SessionContext {
    fn new_session(
        config: &AppConfig,
        workspace: PathBuf,
        provider: ProviderKind,
        args: &[String],
    ) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root.join(workspace_key(&workspace));
        fs::create_dir_all(&workspace_dir)
            .with_context(|| format!("failed to create {}", workspace_dir.display()))?;
        let session_id = generate_session_id(&workspace);
        let session = Self::from_parts(config, workspace, workspace_dir, session_id);
        session.ensure(config)?;
        session.save_session_meta(provider, args)?;
        write_atomic(
            &session.workspace_dir.join("latest-session"),
            session.session_id.as_bytes(),
        )?;
        Ok(session)
    }

    fn latest_for_workspace(config: &AppConfig, workspace: PathBuf) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root.join(workspace_key(&workspace));
        let session_id = fs::read_to_string(workspace_dir.join("latest-session"))
            .context("no session found for this workspace; start `./llm-box copilot` first")?;
        Self::from_session_id(config, workspace, session_id.trim().to_string())
    }

    fn from_session_id(config: &AppConfig, workspace: PathBuf, session_id: String) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root.join(workspace_key(&workspace));
        let session = Self::from_parts(config, workspace, workspace_dir, session_id);
        session.ensure(config)?;
        Ok(session)
    }

    fn from_parts(
        config: &AppConfig,
        workspace: PathBuf,
        workspace_dir: PathBuf,
        session_id: String,
    ) -> Self {
        let session_dir = config.sessions_root.join(&session_id);
        Self {
            session_id,
            workspace,
            workspace_dir,
            session_dir: session_dir.clone(),
            container_home: config.container_home.clone(),
            allowed_hosts_file: session_dir.join("allowed-hosts.txt"),
            pending_log_file: session_dir.join("pending.jsonl"),
            dismissed_file: session_dir.join("dismissed.json"),
            proxy_log_file: session_dir.join("proxy.log"),
            proxy_ready_file: session_dir.join("proxy-ready.json"),
            ui_ready_file: session_dir.join("ui-ready.json"),
            session_meta_file: session_dir.join("session-meta.json"),
        }
    }

    fn ensure(&self, config: &AppConfig) -> Result<()> {
        fs::create_dir_all(self.container_home.join(".copilot"))
            .context("failed to create container auth directory")?;
        fs::create_dir_all(self.container_home.join(".local/state"))
            .context("failed to create container state directory")?;
        fs::create_dir_all(&self.session_dir)
            .with_context(|| format!("failed to create {}", self.session_dir.display()))?;

        if !self.allowed_hosts_file.exists() {
            let mut initial_hosts = DEFAULT_ALLOWED_HOSTS
                .iter()
                .map(|host| host.to_string())
                .collect::<BTreeSet<_>>();
            initial_hosts.extend(config.user_default_hosts()?);
            let initial = initial_hosts
                .into_iter()
                .map(|item| format!("{item}\n"))
                .collect::<String>();
            write_atomic(&self.allowed_hosts_file, initial.as_bytes())?;
        }
        if !self.pending_log_file.exists() {
            fs::File::create(&self.pending_log_file).with_context(|| {
                format!("failed to initialize {}", self.pending_log_file.display())
            })?;
        }
        if !self.dismissed_file.exists() {
            write_atomic(&self.dismissed_file, b"{}\n")?;
        }
        Ok(())
    }

    fn allowed_hosts(&self) -> Result<Vec<String>> {
        let contents = fs::read_to_string(&self.allowed_hosts_file)
            .with_context(|| format!("failed to read {}", self.allowed_hosts_file.display()))?;
        let mut hosts = contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        hosts.sort();
        hosts.dedup();
        Ok(hosts)
    }

    fn pending_items(&self) -> Result<Vec<PendingItem>> {
        let allowed = self.allowed_hosts()?.into_iter().collect::<HashSet<_>>();
        let dismissed = self.dismissed_map()?;
        let contents = fs::read_to_string(&self.pending_log_file)
            .with_context(|| format!("failed to read {}", self.pending_log_file.display()))?;
        let mut latest = BTreeMap::new();
        for (index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: PendingLogEntry = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse pending log line {} in {}",
                    index + 1,
                    self.pending_log_file.display()
                )
            })?;
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

    fn allow_host(&self, host: &str) -> Result<()> {
        let normalized = normalize_host(host)?;
        let mut allowed = self.allowed_hosts()?.into_iter().collect::<BTreeSet<_>>();
        allowed.insert(normalized);
        let contents = allowed
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.allowed_hosts_file, contents.as_bytes())
    }

    fn deny_host(&self, host: &str) -> Result<()> {
        let normalized = normalize_host(host)?;
        let mut allowed = self.allowed_hosts()?.into_iter().collect::<BTreeSet<_>>();
        allowed.remove(&normalized);
        let contents = allowed
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.allowed_hosts_file, contents.as_bytes())
    }

    fn dismiss_host(&self, host: &str) -> Result<()> {
        let normalized = normalize_host(host)?;
        let mut dismissed = self.dismissed_map()?;
        dismissed.insert(normalized, current_epoch_seconds());
        let bytes =
            serde_json::to_vec_pretty(&dismissed).context("failed to serialize dismissed state")?;
        write_atomic(&self.dismissed_file, &[bytes, vec![b'\n']].concat())
    }

    fn dismissed_map(&self) -> Result<BTreeMap<String, u64>> {
        let raw = fs::read_to_string(&self.dismissed_file)
            .with_context(|| format!("failed to read {}", self.dismissed_file.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.dismissed_file.display()))
    }

    fn save_session_meta(&self, provider: ProviderKind, args: &[String]) -> Result<()> {
        let payload = SessionMeta {
            session_id: self.session_id.clone(),
            workspace: self.workspace.display().to_string(),
            provider: provider.command_name().to_string(),
            last_started_epoch: current_epoch_seconds(),
            last_invocation: args.to_vec(),
        };
        let data =
            serde_json::to_vec_pretty(&payload).context("failed to serialize session metadata")?;
        write_atomic(&self.session_meta_file, &[data, vec![b'\n']].concat())
    }
}

fn generate_session_id(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    hasher.update(current_epoch_seconds().to_string().as_bytes());
    hasher.update(std::process::id().to_string().as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

pub(crate) fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn workspace_key(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| format!("failed to replace {}", path.display()))
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionMeta {
    pub(crate) session_id: String,
    pub(crate) workspace: String,
    pub(crate) provider: String,
    pub(crate) last_started_epoch: u64,
    pub(crate) last_invocation: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PendingLogEntry {
    pub(crate) timestamp: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PendingItem {
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
    pub(crate) timestamp: String,
    #[serde(skip)]
    pub(crate) epoch: u64,
}

#[derive(Debug)]
struct ProxyProcess {
    child: Child,
    ready_file: PathBuf,
    port: u16,
}

impl ProxyProcess {
    fn start(session: &SessionContext) -> Result<Self> {
        let _ = fs::remove_file(&session.proxy_ready_file);

        let log_file = fs::File::create(&session.proxy_log_file)
            .with_context(|| format!("failed to create {}", session.proxy_log_file.display()))?;
        let log_file_err = log_file
            .try_clone()
            .context("failed to clone proxy log handle")?;

        let executable = env::current_exe().context("failed to resolve current executable")?;
        let mut child = Command::new(executable)
            .arg("__proxy")
            .args([
                "--listen-host",
                "127.0.0.1",
                "--listen-port",
                "0",
                "--allowed-hosts-file",
            ])
            .arg(&session.allowed_hosts_file)
            .arg("--pending-log-file")
            .arg(&session.pending_log_file)
            .arg("--workspace")
            .arg(session.workspace.display().to_string())
            .arg("--ready-file")
            .arg(&session.proxy_ready_file)
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .spawn()
            .context("failed to start proxy process")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(payload) = fs::read_to_string(&session.proxy_ready_file) {
                if let Ok(info) = serde_json::from_str::<ProxyReady>(&payload) {
                    if TcpStream::connect(("127.0.0.1", info.listen_port)).is_ok() {
                        return Ok(Self {
                            child,
                            ready_file: session.proxy_ready_file.clone(),
                            port: info.listen_port,
                        });
                    }
                }
            }

            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!(
                    "egress proxy failed to become ready; see {}",
                    session.proxy_log_file.display()
                );
            }

            if child
                .try_wait()
                .context("failed to poll proxy process")?
                .is_some()
            {
                bail!(
                    "egress proxy exited before becoming ready; see {}",
                    session.proxy_log_file.display()
                );
            }

            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ready_file);
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
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
        Ok(Some(Self::start(session)?))
    }

    fn start(session: &SessionContext) -> Result<Self> {
        let _ = fs::remove_file(&session.ui_ready_file);
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
            .arg(&session.session_dir)
            .arg("--ready-file")
            .arg(&session.ui_ready_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start browser companion")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(payload) = fs::read_to_string(&session.ui_ready_file) {
                if let Ok(info) = serde_json::from_str::<UiReady>(&payload) {
                    let url = format!("http://127.0.0.1:{}/", info.listen_port);
                    open_browser(&url)?;
                    return Ok(Self {
                        child,
                        ready_file: session.ui_ready_file.clone(),
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
        let _ = &self.url;
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
    let _ = command.status().context("failed to launch browser")?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProxyReady {
    pub(crate) listen_port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UiReady {
    pub(crate) listen_port: u16,
}

pub(crate) fn normalize_host(value: &str) -> Result<String> {
    let mut value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        bail!("invalid hostname");
    }

    if let Some((_, remainder)) = value.split_once("://") {
        value = remainder.to_string();
    }
    if let Some((_, remainder)) = value.rsplit_once('@') {
        value = remainder.to_string();
    }
    if let Some((head, _)) = value.split_once('/') {
        value = head.to_string();
    }
    if let Some(stripped) = value.strip_prefix('[') {
        if let Some((host, _)) = stripped.split_once(']') {
            value = host.to_string();
        }
    } else if let Some((host, _)) = value.split_once(':') {
        value = host.to_string();
    }
    value = value.trim_matches('.').to_string();
    if value.is_empty() {
        bail!("invalid hostname");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn normalize_host_strips_scheme_path_and_port() {
        assert_eq!(
            normalize_host("https://Objects.GitHubusercontent.com:443/foo").unwrap(),
            "objects.githubusercontent.com"
        );
    }

    #[test]
    fn normalize_host_handles_plain_host() {
        assert_eq!(normalize_host("github.com").unwrap(), "github.com");
    }

    #[test]
    fn workspace_key_is_stable() {
        let path = PathBuf::from("/tmp/example");
        assert_eq!(workspace_key(&path), workspace_key(&path));
    }

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
    fn user_defaults_round_trip_normalizes_and_dedups() {
        let root = unique_test_dir("user-defaults-round-trip");
        let config = test_config(&root);

        config
            .add_user_default_host("https://Defaults.Example:443/path")
            .unwrap();
        config.add_user_default_host("defaults.example").unwrap();

        assert_eq!(
            config.user_default_hosts().unwrap(),
            vec!["defaults.example"]
        );

        config.remove_user_default_host("defaults.example").unwrap();
        assert!(config.user_default_hosts().unwrap().is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn new_sessions_include_user_defaults() {
        let root = unique_test_dir("session-seeds-user-defaults");
        let config = test_config(&root);
        let workspace = root.join("workspace-a");
        fs::create_dir_all(&workspace).unwrap();
        config.add_user_default_host("seeded.example").unwrap();

        let session =
            SessionContext::new_session(&config, workspace, ProviderKind::Copilot, &[]).unwrap();
        let allowed = session.allowed_hosts().unwrap();

        assert!(allowed.contains(&"seeded.example".to_string()));
        assert!(allowed.contains(&"api.github.com".to_string()));
        assert!(allowed.contains(&"api.business.githubcopilot.com".to_string()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn built_in_default_allowlist_is_minimal() {
        assert_eq!(
            DEFAULT_ALLOWED_HOSTS,
            &["api.github.com", "api.business.githubcopilot.com"]
        );
    }

    #[test]
    fn existing_sessions_are_not_backfilled_by_new_user_defaults() {
        let root = unique_test_dir("existing-session-no-backfill");
        let config = test_config(&root);
        let workspace = root.join("workspace-b");
        fs::create_dir_all(&workspace).unwrap();

        let session =
            SessionContext::new_session(&config, workspace.clone(), ProviderKind::Copilot, &[])
                .unwrap();
        assert!(
            !session
                .allowed_hosts()
                .unwrap()
                .contains(&"later.example".to_string())
        );

        config.add_user_default_host("later.example").unwrap();
        let reopened =
            SessionContext::from_session_id(&config, workspace, session.session_id.clone())
                .unwrap();

        assert!(
            !reopened
                .allowed_hosts()
                .unwrap()
                .contains(&"later.example".to_string())
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_config(root: &Path) -> AppConfig {
        AppConfig {
            repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            config_root: root.to_path_buf(),
            workspaces_root: root.join("workspaces"),
            sessions_root: root.join("sessions"),
            container_home: root.join("container-home"),
            user_defaults_file: root.join("default-allowed-hosts.txt"),
            image_name: "llm-box".to_string(),
        }
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "llm-box-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}

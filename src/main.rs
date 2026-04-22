mod broker;
mod config;
mod egress;
mod runtime;
mod session;
mod test_support;
mod ui;
mod webui;

use crate::config::AppConfig;
use crate::egress::parse_target_spec;
use crate::runtime::{
    build_image, init_repo_overlay_dockerfile, request_connector_endpoint_from_broker, run_copilot,
};
use crate::session::SessionContext;
use crate::ui::{ApprovalsHub, open_browser};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::env;
use std::path::{Path, PathBuf};

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
    #[command(hide = true, name = "__ui-hub")]
    UiHub(HubUiArgs),
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
pub(crate) struct HubUiArgs {
    #[arg(long)]
    pub(crate) listen_host: String,
    #[arg(long)]
    pub(crate) listen_port: u16,
    #[arg(long)]
    pub(crate) sessions_root: PathBuf,
    #[arg(long)]
    pub(crate) ready_file: PathBuf,
    #[arg(long)]
    pub(crate) activity_file: PathBuf,
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
            let focus_session = preferred_ui_session(&config, args.session)?;
            let url = ApprovalsHub::ensure_started(&config)?.url(focus_session.as_deref());
            println!("{url}");
            if env::var_os("LLM_BOX_NO_BROWSER").is_none() {
                open_browser(&url)
                    .with_context(|| format!("failed to open approvals hub at {url}"))?;
            }
            Ok(0)
        }
        Commands::Copilot(args) => {
            let config = AppConfig::detect()?;
            run_copilot(&config, &args.args)
        }
        Commands::Broker(args) => broker::run_broker_command(args),
        Commands::UiHub(args) => webui::run_ui_hub_command(args),
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

fn preferred_ui_session(
    config: &AppConfig,
    explicit_session: Option<String>,
) -> Result<Option<String>> {
    if let Some(session_id) = explicit_session {
        ensure_session_exists(config, &session_id)?;
        return Ok(Some(session_id));
    }

    let Ok(workspace) = env::current_dir() else {
        return Ok(None);
    };
    match SessionContext::latest_for_workspace(config, workspace) {
        Ok(session) => Ok(Some(session.session_id().to_string())),
        Err(_) => Ok(None),
    }
}

fn ensure_session_exists(config: &AppConfig, session_id: &str) -> Result<()> {
    let mut components = Path::new(session_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) if !session_id.is_empty() => {}
        _ => bail!("invalid session `{session_id}`"),
    }
    let session_dir = config.sessions_root().join(session_id);
    if !session_dir.join("session-meta.json").is_file() {
        bail!("unknown session `{session_id}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

use crate::config::AppConfig;
use crate::egress::{
    AllowedItem, ConnectorMapping, EgressTarget, PendingItem, PendingLogEntry,
    ensure_connector_mapping, parse_target_spec, read_allowed_targets_file,
    read_connector_mappings_file, remove_connector_mapping, serialize_connector_mappings,
    serialize_target_set,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_ALLOWED_TARGETS: &[&str] = &[
    "https://api.github.com:443",
    "https://api.business.githubcopilot.com:443",
    "https://telemetry.business.githubcopilot.com:443",
];
const COPILOT_PROVIDER: &str = "copilot";

const FILE_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const FILE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const FILE_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub(crate) struct SessionContext {
    session_id: String,
    workspace: PathBuf,
    workspace_dir: PathBuf,
    workspace_home: PathBuf,
    store: SessionStore,
    broker_log_file: PathBuf,
    broker_ready_file: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ActiveSessionLease {
    active_process_file: PathBuf,
}

impl Drop for ActiveSessionLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.active_process_file);
    }
}

impl SessionContext {
    pub(crate) fn new_session(
        config: &AppConfig,
        workspace: PathBuf,
        args: &[String],
    ) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root().join(workspace_key(&workspace));
        fs::create_dir_all(&workspace_dir)
            .with_context(|| format!("failed to create {}", workspace_dir.display()))?;
        let session_id = generate_session_id(&workspace);
        let session = Self::from_parts(config, workspace, workspace_dir, session_id);
        session.ensure(config)?;
        session.save_session_meta(args)?;
        write_atomic(
            &session.workspace_dir.join("latest-session"),
            session.session_id.as_bytes(),
        )?;
        Ok(session)
    }

    pub(crate) fn latest_for_workspace(config: &AppConfig, workspace: PathBuf) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root().join(workspace_key(&workspace));
        let session_id = fs::read_to_string(workspace_dir.join("latest-session"))
            .context("no session found for this workspace; start `./llm-box copilot` first")?;
        Self::from_session_id(config, workspace, session_id.trim().to_string())
    }

    pub(crate) fn from_session_id(
        config: &AppConfig,
        workspace: PathBuf,
        session_id: String,
    ) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("failed to canonicalize {}", workspace.display()))?;
        let workspace_dir = config.workspaces_root().join(workspace_key(&workspace));
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
        let session_dir = config.sessions_root().join(&session_id);
        let store = SessionStore::from_dir(session_dir.clone());
        let workspace_home = workspace_home_path(&workspace_dir);
        Self {
            session_id,
            workspace,
            workspace_dir,
            workspace_home,
            store,
            broker_log_file: session_dir.join("broker.log"),
            broker_ready_file: session_dir.join("broker-ready.json"),
        }
    }

    fn ensure(&self, config: &AppConfig) -> Result<()> {
        fs::create_dir_all(&self.workspace_dir)
            .with_context(|| format!("failed to create {}", self.workspace_dir.display()))?;
        fs::create_dir_all(self.workspace_home.join(".copilot"))
            .context("failed to create container auth directory")?;
        fs::create_dir_all(self.workspace_home.join(".local/state"))
            .context("failed to create container state directory")?;
        self.store.initialize(config)
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) fn workspace_home(&self) -> &Path {
        &self.workspace_home
    }

    pub(crate) fn session_dir(&self) -> &Path {
        self.store.session_dir()
    }

    pub(crate) fn broker_log_file(&self) -> &Path {
        &self.broker_log_file
    }

    pub(crate) fn broker_ready_file(&self) -> &Path {
        &self.broker_ready_file
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &SessionStore {
        &self.store
    }

    #[cfg(test)]
    pub(crate) fn allowed_target_specs(&self) -> Result<Vec<String>> {
        self.store.allowed_target_specs()
    }

    pub(crate) fn allowed_items(&self) -> Result<Vec<AllowedItem>> {
        self.store.allowed_items()
    }

    pub(crate) fn pending_items(&self) -> Result<Vec<PendingItem>> {
        self.store.pending_items()
    }

    pub(crate) fn allow_target(&self, target: &str) -> Result<()> {
        self.store.allow_target(target)
    }

    pub(crate) fn deny_target(&self, target: &str) -> Result<()> {
        self.store.deny_target(target)
    }

    pub(crate) fn dismiss_target(&self, target: &str) -> Result<()> {
        self.store.dismiss_target(target)
    }

    pub(crate) fn connector_endpoint(&self, target: &str) -> Result<ConnectorMapping> {
        self.store.ensure_connector_endpoint(target)
    }

    pub(crate) fn mark_active(&self) -> Result<ActiveSessionLease> {
        self.store.mark_active()
    }

    fn save_session_meta(&self, args: &[String]) -> Result<()> {
        self.store.write_session_meta(&SessionMeta {
            session_id: self.session_id.clone(),
            workspace: self.workspace.display().to_string(),
            provider: COPILOT_PROVIDER.to_string(),
            last_started_epoch: current_epoch_seconds(),
            last_invocation: args.to_vec(),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    session_dir: PathBuf,
    allowed_targets_file: PathBuf,
    connectors_file: PathBuf,
    pending_events_file: PathBuf,
    dismissed_file: PathBuf,
    session_meta_file: PathBuf,
    active_process_file: PathBuf,
}

impl SessionStore {
    pub(crate) fn from_dir(session_dir: PathBuf) -> Self {
        Self {
            allowed_targets_file: session_dir.join("allowed-targets.txt"),
            connectors_file: session_dir.join("connectors.json"),
            pending_events_file: session_dir.join("pending-events.jsonl"),
            dismissed_file: session_dir.join("dismissed.json"),
            session_meta_file: session_dir.join("session-meta.json"),
            active_process_file: session_dir.join("active-process.json"),
            session_dir,
        }
    }

    pub(crate) fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    #[cfg(test)]
    pub(crate) fn allowed_targets_file(&self) -> &Path {
        &self.allowed_targets_file
    }

    #[cfg(test)]
    pub(crate) fn pending_events_file(&self) -> &Path {
        &self.pending_events_file
    }

    #[cfg(test)]
    pub(crate) fn connectors_file(&self) -> &Path {
        &self.connectors_file
    }

    #[cfg(test)]
    pub(crate) fn active_process_file(&self) -> &Path {
        &self.active_process_file
    }

    pub(crate) fn initialize(&self, config: &AppConfig) -> Result<()> {
        fs::create_dir_all(&self.session_dir)
            .with_context(|| format!("failed to create {}", self.session_dir.display()))?;
        if !self.allowed_targets_file.exists() {
            let mut initial_targets = DEFAULT_ALLOWED_TARGETS
                .iter()
                .map(|target| parse_target_spec(target))
                .collect::<Result<BTreeSet<_>>>()?;
            initial_targets.extend(
                config
                    .user_default_targets()?
                    .into_iter()
                    .map(|item| parse_target_spec(&item))
                    .collect::<Result<BTreeSet<_>>>()?,
            );
            let initial = serialize_target_set(initial_targets);
            write_atomic(&self.allowed_targets_file, initial.as_bytes())?;
        }
        if !self.connectors_file.exists() {
            write_atomic(&self.connectors_file, b"[]\n")?;
        }
        if !self.pending_events_file.exists() {
            fs::File::create(&self.pending_events_file).with_context(|| {
                format!(
                    "failed to initialize {}",
                    self.pending_events_file.display()
                )
            })?;
        }
        if !self.dismissed_file.exists() {
            write_atomic(&self.dismissed_file, b"{}\n")?;
        }
        Ok(())
    }

    pub(crate) fn load_state(&self) -> Result<SessionState> {
        Ok(SessionState {
            session: self.session_meta()?,
            pending: self.pending_items()?,
            allowed: self.allowed_items()?,
        })
    }

    pub(crate) fn is_active_session(&self) -> Result<bool> {
        if !self.active_process_file.exists() {
            return Ok(false);
        }
        let raw = match fs::read_to_string(&self.active_process_file) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read {}", self.active_process_file.display())
                });
            }
        };
        let info: ActiveProcessInfo = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.active_process_file.display()))?;
        if process_is_running(info.pid) {
            Ok(true)
        } else {
            remove_file_if_exists(&self.active_process_file)?;
            Ok(false)
        }
    }

    pub(crate) fn session_meta(&self) -> Result<SessionMeta> {
        let raw = fs::read_to_string(&self.session_meta_file)
            .with_context(|| format!("failed to read {}", self.session_meta_file.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.session_meta_file.display()))
    }

    pub(crate) fn allowed_target_specs(&self) -> Result<Vec<String>> {
        Ok(self
            .allowed_targets()?
            .into_iter()
            .map(|target| target.to_string())
            .collect())
    }

    pub(crate) fn allowed_items(&self) -> Result<Vec<AllowedItem>> {
        let connectors = self.connector_endpoint_map()?;
        Ok(self
            .allowed_targets()?
            .into_iter()
            .map(|target| {
                let key = target.to_string();
                AllowedItem::from_target(&target, connectors.get(&key).cloned())
            })
            .collect())
    }

    pub(crate) fn pending_items(&self) -> Result<Vec<PendingItem>> {
        let allowed = self
            .allowed_target_specs()?
            .into_iter()
            .collect::<HashSet<_>>();
        let dismissed = self.dismissed_map()?;
        let connectors = self.connector_endpoint_map()?;
        let contents = with_file_lock(&self.pending_events_file, || {
            fs::read_to_string(&self.pending_events_file)
                .with_context(|| format!("failed to read {}", self.pending_events_file.display()))
        })?;
        let mut latest = BTreeMap::new();
        let lines = contents.lines().collect::<Vec<_>>();
        let trailing_record_may_be_incomplete = !contents.ends_with('\n');
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: PendingLogEntry = match serde_json::from_str(line) {
                Ok(event) => event,
                Err(_) if trailing_record_may_be_incomplete && index + 1 == lines.len() => break,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to parse pending log line {} in {}",
                            index + 1,
                            self.pending_events_file.display()
                        )
                    });
                }
            };
            let target = event.target()?;
            let key = target.to_string();
            let epoch = event.event_epoch_nanos.parse::<u64>().unwrap_or(0);
            latest.insert(
                key.clone(),
                PendingItem::from_target(
                    &target,
                    connectors.get(&key).cloned(),
                    event.event_epoch_nanos,
                    epoch,
                ),
            );
        }
        let mut items = latest
            .into_values()
            .filter(|item| !allowed.contains(&item.target))
            .filter(|item| dismissed.get(&item.target).copied().unwrap_or(0) < item.epoch)
            .collect::<Vec<_>>();
        items.sort_by(|a, b| b.epoch.cmp(&a.epoch).then_with(|| a.target.cmp(&b.target)));
        Ok(items)
    }

    pub(crate) fn allow_target(&self, target: &str) -> Result<()> {
        let target = parse_target_spec(target)?;
        let target_key = target.to_string();
        if target.uses_connector() {
            let _ = self.ensure_connector_for_target(&target)?;
        }
        self.update_allowed_targets(move |targets| {
            targets.insert(target);
            Ok(())
        })?;
        self.acknowledge_target(&target_key)
    }

    pub(crate) fn deny_target(&self, target: &str) -> Result<()> {
        let target = parse_target_spec(target)?;
        let target_key = target.to_string();
        let denied_target = target.clone();
        self.update_allowed_targets(move |targets| {
            targets.remove(&denied_target);
            Ok(())
        })?;
        if target.uses_connector() {
            self.remove_connector_for_target(&target)?;
        }
        self.acknowledge_target(&target_key)
    }

    pub(crate) fn dismiss_target(&self, target: &str) -> Result<()> {
        let target = parse_target_spec(target)?.to_string();
        self.acknowledge_target(&target)
    }

    fn acknowledge_target(&self, target: &str) -> Result<()> {
        with_file_lock(&self.dismissed_file, || {
            let mut dismissed = self.dismissed_map()?;
            dismissed.insert(target.to_string(), current_epoch_nanos());
            let bytes = serde_json::to_vec_pretty(&dismissed)
                .context("failed to serialize dismissed state")?;
            write_atomic(&self.dismissed_file, &[bytes, vec![b'\n']].concat())
        })
    }

    pub(crate) fn ensure_connector_endpoint(&self, target: &str) -> Result<ConnectorMapping> {
        let target = parse_target_spec(target)?;
        self.ensure_connector_for_target(&target)
    }

    pub(crate) fn write_session_meta(&self, payload: &SessionMeta) -> Result<()> {
        let data =
            serde_json::to_vec_pretty(payload).context("failed to serialize session metadata")?;
        write_atomic(&self.session_meta_file, &[data, vec![b'\n']].concat())
    }

    fn allowed_targets(&self) -> Result<Vec<EgressTarget>> {
        read_allowed_targets_file(&self.allowed_targets_file)
    }

    fn connector_endpoint_map(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .connector_mappings()?
            .into_iter()
            .map(|mapping| (mapping.target.to_string(), mapping.endpoint()))
            .collect())
    }

    fn connector_mappings(&self) -> Result<Vec<ConnectorMapping>> {
        read_connector_mappings_file(&self.connectors_file)
    }

    fn mark_active(&self) -> Result<ActiveSessionLease> {
        let payload = ActiveProcessInfo {
            pid: process::id(),
            started_epoch: current_epoch_seconds(),
        };
        let bytes =
            serde_json::to_vec_pretty(&payload).context("failed to serialize active process")?;
        write_atomic(&self.active_process_file, &[bytes, vec![b'\n']].concat())?;
        Ok(ActiveSessionLease {
            active_process_file: self.active_process_file.clone(),
        })
    }

    fn ensure_connector_for_target(&self, target: &EgressTarget) -> Result<ConnectorMapping> {
        with_file_lock(&self.connectors_file, || {
            let mut mappings = self.connector_mappings()?;
            let mapping = ensure_connector_mapping(&mut mappings, target)?;
            let bytes = serialize_connector_mappings(&mappings)?;
            write_atomic(&self.connectors_file, &bytes)?;
            Ok(mapping)
        })
    }

    fn remove_connector_for_target(&self, target: &EgressTarget) -> Result<()> {
        with_file_lock(&self.connectors_file, || {
            let mut mappings = self.connector_mappings()?;
            if remove_connector_mapping(&mut mappings, target) {
                let bytes = serialize_connector_mappings(&mappings)?;
                write_atomic(&self.connectors_file, &bytes)?;
            }
            Ok(())
        })
    }

    fn dismissed_map(&self) -> Result<BTreeMap<String, u64>> {
        let raw = fs::read_to_string(&self.dismissed_file)
            .with_context(|| format!("failed to read {}", self.dismissed_file.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.dismissed_file.display()))
    }

    fn update_allowed_targets<F>(&self, update: F) -> Result<()>
    where
        F: FnOnce(&mut BTreeSet<EgressTarget>) -> Result<()>,
    {
        with_file_lock(&self.allowed_targets_file, || {
            let mut targets = self.allowed_targets()?.into_iter().collect::<BTreeSet<_>>();
            update(&mut targets)?;
            let contents = serialize_target_set(targets);
            write_atomic(&self.allowed_targets_file, contents.as_bytes())
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionState {
    pub(crate) session: SessionMeta,
    pub(crate) pending: Vec<PendingItem>,
    pub(crate) allowed: Vec<AllowedItem>,
}

pub(crate) fn with_file_lock<T>(target: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = acquire_file_lock(target)?;
    action()
}

fn acquire_file_lock(target: &Path) -> Result<FileLock> {
    let lock_path = lock_path_for(target);
    let deadline = Instant::now() + FILE_LOCK_TIMEOUT;
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = write_lock_metadata(&mut file, &lock_path) {
                    let _ = fs::remove_file(&lock_path);
                    return Err(error);
                }
                return Ok(FileLock {
                    _file: file,
                    path: lock_path,
                });
            }
            Err(error)
                if error.kind() == ErrorKind::AlreadyExists
                    && try_remove_stale_lock(&lock_path)? =>
            {
                continue;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists && Instant::now() < deadline => {
                thread::sleep(FILE_LOCK_RETRY_INTERVAL);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                bail!("timed out waiting for lock {}", lock_path.display());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", lock_path.display()));
            }
        }
    }
}

fn try_remove_stale_lock(lock_path: &Path) -> Result<bool> {
    let contents = match fs::read_to_string(lock_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", lock_path.display()));
        }
    };

    let created_epoch = match contents.lines().nth(1) {
        Some(value) => value.trim().parse::<u64>().with_context(|| {
            format!(
                "failed to parse {}; invalid lock timestamp",
                lock_path.display()
            )
        })?,
        None if lock_file_age(lock_path)? < FILE_LOCK_STALE_AFTER => return Ok(false),
        None => {
            bail!(
                "failed to parse {}; missing lock timestamp",
                lock_path.display()
            )
        }
    };
    let is_stale =
        current_epoch_seconds().saturating_sub(created_epoch) >= FILE_LOCK_STALE_AFTER.as_secs();
    if !is_stale {
        return Ok(false);
    }

    match fs::remove_file(lock_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", lock_path.display()))
        }
    }
}

fn lock_file_age(path: &Path) -> Result<Duration> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("failed to inspect mtime for {}", path.display()))?;
    SystemTime::now()
        .duration_since(modified)
        .with_context(|| format!("lock file mtime is in the future for {}", path.display()))
}

fn write_lock_metadata(file: &mut fs::File, lock_path: &Path) -> Result<()> {
    writeln!(file, "{}", process::id())
        .with_context(|| format!("failed to write pid to {}", lock_path.display()))?;
    writeln!(file, "{}", current_epoch_seconds())
        .with_context(|| format!("failed to write timestamp to {}", lock_path.display()))?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn lock_path_for(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .unwrap_or_else(|| OsStr::new("llm-box-state"))
        .to_string_lossy();
    parent.join(format!(".{file_name}.lock"))
}

#[derive(Debug)]
struct FileLock {
    _file: fs::File,
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn generate_session_id(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    hasher.update(current_epoch_nanos().to_string().as_bytes());
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(
        TEMP_FILE_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())[..16].to_string()
}

pub(crate) fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn current_epoch_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub(crate) fn workspace_key(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    format!("{:x}", hasher.finalize())
}

fn workspace_home_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("home")
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("llm-box-state"))
        .to_string_lossy();
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| format!("failed to replace {}", path.display()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionMeta {
    pub(crate) session_id: String,
    pub(crate) workspace: String,
    pub(crate) provider: String,
    pub(crate) last_started_epoch: u64,
    pub(crate) last_invocation: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveProcessInfo {
    pid: u32,
    started_epoch: u64,
}

#[cfg(unix)]
pub(crate) fn process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn process_is_running(pid: u32) -> bool {
    pid == process::id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn normalize_host_strips_scheme_path_and_port() {
        assert_eq!(
            crate::egress::normalize_host("https://Objects.GitHubusercontent.com:443/foo").unwrap(),
            "objects.githubusercontent.com"
        );
    }

    #[test]
    fn normalize_host_handles_plain_host() {
        assert_eq!(
            crate::egress::normalize_host("github.com").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn normalize_host_preserves_ipv6_literals() {
        assert_eq!(
            crate::egress::normalize_host("2001:db8::1").unwrap(),
            "2001:db8::1"
        );
        assert_eq!(
            crate::egress::normalize_host("[2001:db8::1]:443").unwrap(),
            "2001:db8::1"
        );
    }

    #[test]
    fn normalize_host_rejects_unsafe_characters() {
        assert!(crate::egress::normalize_host("bad host").is_err());
        assert!(crate::egress::normalize_host("bad'host").is_err());
    }

    #[test]
    fn workspace_key_is_stable() {
        let path = PathBuf::from("/tmp/example");
        assert_eq!(workspace_key(&path), workspace_key(&path));
    }

    #[test]
    fn new_sessions_include_user_defaults() {
        let root = unique_test_dir("session-seeds-user-defaults");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace-a");
        fs::create_dir_all(&workspace).unwrap();
        config
            .add_user_default_target("https://seeded.example")
            .unwrap();

        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();
        let allowed = session.allowed_target_specs().unwrap();

        assert!(allowed.contains(&"https://seeded.example:443".to_string()));
        assert!(allowed.contains(&"https://api.github.com:443".to_string()));
        assert!(allowed.contains(&"https://api.business.githubcopilot.com:443".to_string()));
        assert!(allowed.contains(&"https://telemetry.business.githubcopilot.com:443".to_string()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn built_in_default_allowlist_is_minimal() {
        assert_eq!(
            DEFAULT_ALLOWED_TARGETS,
            &[
                "https://api.github.com:443",
                "https://api.business.githubcopilot.com:443",
                "https://telemetry.business.githubcopilot.com:443",
            ]
        );
    }

    #[test]
    fn existing_sessions_are_not_backfilled_by_new_user_defaults() {
        let root = unique_test_dir("existing-session-no-backfill");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace-b");
        fs::create_dir_all(&workspace).unwrap();

        let session = SessionContext::new_session(&config, workspace.clone(), &[]).unwrap();
        assert!(
            !session
                .allowed_target_specs()
                .unwrap()
                .contains(&"https://later.example:443".to_string())
        );

        config
            .add_user_default_target("https://later.example")
            .unwrap();
        let reopened =
            SessionContext::from_session_id(&config, workspace, session.session_id().to_string())
                .unwrap();

        assert!(
            !reopened
                .allowed_target_specs()
                .unwrap()
                .contains(&"https://later.example:443".to_string())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_store_reports_pending_line_numbers() {
        let root = unique_test_dir("invalid-pending-log");
        let store = session_store_fixture(&root);
        fs::write(store.allowed_targets_file(), "https://api.github.com:443\n").unwrap();
        fs::write(
            store.pending_events_file(),
            "{\"event_epoch_nanos\":\"1\",\"kind\":\"https\",\"host\":\"ok.example\",\"port\":443}\nnot-json\n",
        )
        .unwrap();
        fs::write(store.session_dir().join("dismissed.json"), "{}\n").unwrap();
        store
            .write_session_meta(&SessionMeta {
                session_id: "session-1".to_string(),
                workspace: "/tmp/workspace".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 1,
                last_invocation: Vec::new(),
            })
            .unwrap();

        let error = store.pending_items().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to parse pending log line 2")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_store_ignores_incomplete_trailing_pending_record() {
        let root = unique_test_dir("incomplete-pending-log");
        let store = session_store_fixture(&root);
        fs::write(store.allowed_targets_file(), "https://api.github.com:443\n").unwrap();
        fs::write(
            store.pending_events_file(),
            "{\"event_epoch_nanos\":\"1\",\"kind\":\"https\",\"host\":\"ok.example\",\"port\":443}\n{\"event_epoch_nanos\":\"2\",\"kind\":\"https\",\"host\":\"half",
        )
        .unwrap();
        fs::write(store.session_dir().join("dismissed.json"), "{}\n").unwrap();
        store
            .write_session_meta(&SessionMeta {
                session_id: "session-1".to_string(),
                workspace: "/tmp/workspace".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 1,
                last_invocation: Vec::new(),
            })
            .unwrap();

        let pending = store.pending_items().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target, "https://ok.example:443");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_store_reports_invalid_dismissed_state() {
        let root = unique_test_dir("invalid-dismissed-state");
        let store = session_store_fixture(&root);
        fs::write(store.allowed_targets_file(), "https://api.github.com:443\n").unwrap();
        fs::write(store.pending_events_file(), "").unwrap();
        fs::write(store.session_dir().join("dismissed.json"), "{not-json}\n").unwrap();
        store
            .write_session_meta(&SessionMeta {
                session_id: "session-1".to_string(),
                workspace: "/tmp/workspace".to_string(),
                provider: "copilot".to_string(),
                last_started_epoch: 1,
                last_invocation: Vec::new(),
            })
            .unwrap();

        let error = store.load_state().unwrap_err();
        assert!(error.to_string().contains("dismissed.json"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_allow_updates_preserve_all_hosts() {
        let root = unique_test_dir("concurrent-allow-updates");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();
        let store = session.store().clone();
        let targets = vec![
            "https://one.example",
            "https://two.example",
            "https://three.example",
            "https://four.example",
            "https://five.example",
        ];

        let handles = targets
            .iter()
            .map(|target| {
                let store = store.clone();
                let target = target.to_string();
                thread::spawn(move || store.allow_target(&target))
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let allowed = store.allowed_target_specs().unwrap();
        for target in targets {
            assert!(allowed.contains(&format!("{target}:443")));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn connector_endpoint_is_stable_for_target() {
        let root = unique_test_dir("connector-endpoint-stable");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace-connector");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();

        let first = session
            .connector_endpoint("tcp://db.example.internal:5432")
            .unwrap();
        let second = session
            .connector_endpoint("tcp://db.example.internal:5432")
            .unwrap();

        assert_eq!(first.listen_port, second.listen_port);
        let raw = fs::read_to_string(session.store().connectors_file()).unwrap();
        assert!(raw.contains("db.example.internal"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn session_store_state_machine_requires_new_block_after_allow_or_dismiss() {
        let root = unique_test_dir("session-state-machine");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();
        let store = session.store().clone();
        let target = "https://pending.example";

        append_pending_event(&store, target, current_epoch_nanos() + 1);
        assert_eq!(
            pending_targets(&session),
            vec!["https://pending.example:443"]
        );

        session.allow_target(target).unwrap();
        assert!(pending_targets(&session).is_empty());
        assert!(
            session
                .allowed_target_specs()
                .unwrap()
                .contains(&"https://pending.example:443".to_string())
        );

        session.deny_target(target).unwrap();
        assert!(pending_targets(&session).is_empty());
        assert!(
            !session
                .allowed_target_specs()
                .unwrap()
                .contains(&"https://pending.example:443".to_string())
        );

        append_pending_event(&store, target, current_epoch_nanos() + 1);
        assert_eq!(
            pending_targets(&session),
            vec!["https://pending.example:443"]
        );

        session.dismiss_target(target).unwrap();
        assert!(pending_targets(&session).is_empty());

        append_pending_event(&store, target, current_epoch_nanos() + 1);
        assert_eq!(
            pending_targets(&session),
            vec!["https://pending.example:443"]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_session_tracks_live_wrapper_process() {
        let root = unique_test_dir("active-session-live");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();

        {
            let _lease = session.mark_active().unwrap();
            assert!(session.store().is_active_session().unwrap());
        }

        assert!(!session.store().is_active_session().unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_session_ignores_stale_process_markers() {
        let root = unique_test_dir("active-session-stale");
        let store = session_store_fixture(&root);
        fs::write(
            store.active_process_file(),
            serde_json::to_vec(&ActiveProcessInfo {
                pid: 999_999,
                started_epoch: current_epoch_seconds(),
            })
            .unwrap(),
        )
        .unwrap();

        assert!(!store.is_active_session().unwrap());
        assert!(!store.active_process_file().exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_session_reports_invalid_process_markers() {
        let root = unique_test_dir("active-session-invalid");
        let store = session_store_fixture(&root);
        fs::write(store.active_process_file(), "{not-json}\n").unwrap();

        let error = store.is_active_session().unwrap_err();

        assert!(error.to_string().contains("failed to parse"));
        assert!(store.active_process_file().exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deny_target_removes_connector_mapping() {
        let root = unique_test_dir("deny-removes-connector");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();

        session
            .connector_endpoint("tcp://db.example.internal:5432")
            .unwrap();
        assert!(
            fs::read_to_string(session.store().connectors_file())
                .unwrap()
                .contains("db.example.internal")
        );

        session
            .deny_target("tcp://db.example.internal:5432")
            .unwrap();

        assert_eq!(
            fs::read_to_string(session.store().connectors_file()).unwrap(),
            "[]\n"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deny_target_hides_prior_pending_event_until_new_attempt() {
        let root = unique_test_dir("deny-hides-prior-pending");
        let config = AppConfig::for_tests(&root);
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let session = SessionContext::new_session(&config, workspace, &[]).unwrap();
        let target = "tcp://db.example.internal:5432";
        let store = session.store();
        let prior_event = PendingLogEntry {
            event_epoch_nanos: "1".to_string(),
            kind: crate::egress::EgressKind::Tcp,
            host: "db.example.internal".to_string(),
            port: 5432,
        };
        fs::write(
            store.pending_events_file(),
            format!("{}\n", serde_json::to_string(&prior_event).unwrap()),
        )
        .unwrap();

        session.allow_target(target).unwrap();
        assert!(session.pending_items().unwrap().is_empty());

        session.deny_target(target).unwrap();
        assert!(session.pending_items().unwrap().is_empty());

        let new_event = PendingLogEntry {
            event_epoch_nanos: current_epoch_nanos().to_string(),
            kind: crate::egress::EgressKind::Tcp,
            host: "db.example.internal".to_string(),
            port: 5432,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.pending_events_file())
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&new_event).unwrap()).unwrap();

        assert_eq!(
            pending_targets(&session),
            vec!["tcp://db.example.internal:5432".to_string()]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_atomic_uses_unique_temp_files() {
        let root = unique_test_dir("write-atomic-temp-files");
        fs::create_dir_all(&root).unwrap();
        let target = Arc::new(root.join("state.txt"));
        let payloads = (0..8)
            .map(|index| format!("payload-{index}\n"))
            .collect::<Vec<_>>();

        let handles = payloads
            .iter()
            .cloned()
            .map(|payload| {
                let target = Arc::clone(&target);
                thread::spawn(move || {
                    for _ in 0..10 {
                        write_atomic(&target, payload.as_bytes()).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let final_contents = fs::read_to_string(&*target).unwrap();
        assert!(payloads.contains(&final_contents));

        let stray_temp = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(".state.txt.") && name.ends_with(".tmp"));
        assert!(stray_temp.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_file_lock_recovers_stale_lock_file() {
        let root = unique_test_dir("stale-lock-file");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("allowed-targets.txt");
        let lock_path = lock_path_for(&target);
        fs::write(&lock_path, "999999\n0\n").unwrap();

        {
            let _lock = acquire_file_lock(&target).unwrap();
            let contents = fs::read_to_string(&lock_path).unwrap();
            assert!(contents.starts_with(&format!("{}\n", process::id())));
        }

        assert!(!lock_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn acquire_file_lock_reports_invalid_lock_metadata() {
        let root = unique_test_dir("invalid-lock-file");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("allowed-targets.txt");
        let lock_path = lock_path_for(&target);
        fs::write(&lock_path, "999999\nnot-a-timestamp\n").unwrap();

        let error = acquire_file_lock(&target).unwrap_err();

        assert!(error.to_string().contains("invalid lock timestamp"));
        assert!(lock_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    fn session_store_fixture(root: &Path) -> SessionStore {
        let session_dir = root.join("session");
        fs::create_dir_all(&session_dir).unwrap();
        SessionStore::from_dir(session_dir)
    }

    fn append_pending_event(store: &SessionStore, target: &str, event_epoch_nanos: u64) {
        let target = parse_target_spec(target).unwrap();
        let existing = fs::read_to_string(store.pending_events_file()).unwrap_or_default();
        let event = serde_json::to_string(&PendingLogEntry {
            event_epoch_nanos: event_epoch_nanos.to_string(),
            kind: target.kind,
            host: target.host,
            port: target.port,
        })
        .unwrap();
        fs::write(store.pending_events_file(), format!("{existing}{event}\n")).unwrap();
    }

    fn pending_targets(session: &SessionContext) -> Vec<String> {
        session
            .pending_items()
            .unwrap()
            .into_iter()
            .map(|item| item.target)
            .collect()
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

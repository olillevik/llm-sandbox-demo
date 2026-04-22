use crate::egress::parse_target_spec;
use crate::session::write_atomic;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct AppConfig {
    repo_root: PathBuf,
    config_root: PathBuf,
    workspaces_root: PathBuf,
    sessions_root: PathBuf,
    shared_copilot_skills_dir: Option<PathBuf>,
    user_default_targets_file: PathBuf,
    image_name: String,
}

impl AppConfig {
    pub(crate) fn detect() -> Result<Self> {
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
            shared_copilot_skills_dir: detect_shared_copilot_skills_dir(&home)?,
            user_default_targets_file: config_root.join("default-allowed-targets.txt"),
            image_name: env::var("LLM_BOX_IMAGE").unwrap_or_else(|_| "llm-box".to_string()),
            repo_root,
        })
    }

    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub(crate) fn workspaces_root(&self) -> &Path {
        &self.workspaces_root
    }

    pub(crate) fn ui_ready_file(&self) -> PathBuf {
        self.config_root.join("ui-ready.json")
    }

    pub(crate) fn ui_activity_file(&self) -> PathBuf {
        self.config_root.join("ui-activity")
    }

    pub(crate) fn sessions_root(&self) -> &Path {
        &self.sessions_root
    }

    pub(crate) fn shared_copilot_skills_dir(&self) -> Option<&Path> {
        self.shared_copilot_skills_dir.as_deref()
    }

    pub(crate) fn image_name(&self) -> &str {
        &self.image_name
    }

    fn ensure_config_root(&self) -> Result<()> {
        fs::create_dir_all(&self.config_root)
            .with_context(|| format!("failed to create {}", self.config_root.display()))
    }

    pub(crate) fn user_default_targets(&self) -> Result<Vec<String>> {
        if !self.user_default_targets_file.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.user_default_targets_file).with_context(|| {
            format!(
                "failed to read {}",
                self.user_default_targets_file.display()
            )
        })?;
        let mut targets = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_target_spec)
            .map(|target| target.map(|item| item.to_string()))
            .collect::<Result<Vec<_>>>()?;
        targets.sort();
        targets.dedup();
        Ok(targets)
    }

    pub(crate) fn add_user_default_target(&self, target: &str) -> Result<()> {
        self.ensure_config_root()?;
        let mut targets = self
            .user_default_targets()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        targets.insert(parse_target_spec(target)?.to_string());
        let contents = targets
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.user_default_targets_file, contents.as_bytes())
    }

    pub(crate) fn remove_user_default_target(&self, target: &str) -> Result<()> {
        self.ensure_config_root()?;
        let mut targets = self
            .user_default_targets()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        targets.remove(&parse_target_spec(target)?.to_string());
        let contents = targets
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<String>();
        write_atomic(&self.user_default_targets_file, contents.as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn for_tests(root: &Path) -> Self {
        Self {
            repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            config_root: root.to_path_buf(),
            workspaces_root: root.join("workspaces"),
            sessions_root: root.join("sessions"),
            shared_copilot_skills_dir: None,
            user_default_targets_file: root.join("default-allowed-targets.txt"),
            image_name: "llm-box".to_string(),
        }
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

fn detect_shared_copilot_skills_dir(home: &Path) -> Result<Option<PathBuf>> {
    let path = env::var_os("LLM_BOX_SHARED_COPILOT_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".copilot").join("skills"));
    if !path.exists() {
        return Ok(None);
    }
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    if !canonical.is_dir() {
        bail!(
            "shared Copilot skills path is not a directory: {}",
            canonical.display()
        );
    }
    Ok(Some(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_defaults_round_trip_normalizes_and_dedups() {
        let root = unique_test_dir("user-defaults-round-trip");
        let config = AppConfig::for_tests(&root);

        config
            .add_user_default_target("https://Defaults.Example:443/path")
            .unwrap();
        config
            .add_user_default_target("https://defaults.example")
            .unwrap();

        assert_eq!(
            config.user_default_targets().unwrap(),
            vec!["https://defaults.example:443"]
        );

        config
            .remove_user_default_target("https://defaults.example")
            .unwrap();
        assert!(config.user_default_targets().unwrap().is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detect_shared_copilot_skills_dir_returns_none_when_missing() {
        let root = unique_test_dir("missing-shared-skills");
        fs::create_dir_all(&root).unwrap();

        assert!(detect_shared_copilot_skills_dir(&root).unwrap().is_none());

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

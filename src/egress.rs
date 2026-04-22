use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

pub(crate) const BROKER_INTERNAL_HOST: &str = "llm-box-broker";
pub(crate) const CONNECTOR_PORT_START: u16 = 46000;
pub(crate) const CONNECTOR_PORT_END: u16 = 55999;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EgressKind {
    Http,
    Https,
    Tcp,
    Ssh,
    Mcp,
}

impl EgressKind {
    pub(crate) fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
            Self::Tcp | Self::Mcp => 0,
            Self::Ssh => 22,
        }
    }

    pub(crate) fn uses_connector(self) -> bool {
        matches!(self, Self::Tcp | Self::Ssh | Self::Mcp)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Tcp => "tcp",
            Self::Ssh => "ssh",
            Self::Mcp => "mcp",
        }
    }
}

impl fmt::Display for EgressKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct EgressTarget {
    pub(crate) kind: EgressKind,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl EgressTarget {
    pub(crate) fn new(kind: EgressKind, host: impl Into<String>, port: u16) -> Result<Self> {
        let host = normalize_host(&host.into())?;
        if port == 0 && kind.default_port() == 0 {
            bail!("port is required for {kind}");
        }
        Ok(Self {
            kind,
            host,
            port: if port == 0 { kind.default_port() } else { port },
        })
    }

    pub(crate) fn uses_connector(&self) -> bool {
        self.kind.uses_connector()
    }
}

impl fmt::Display for EgressTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}:{}",
            self.kind,
            host_for_uri(&self.host),
            self.port
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConnectorMapping {
    pub(crate) target: EgressTarget,
    pub(crate) listen_port: u16,
}

impl ConnectorMapping {
    pub(crate) fn endpoint(&self) -> String {
        connector_endpoint(self.listen_port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingLogEntry {
    pub(crate) event_epoch_nanos: String,
    pub(crate) kind: EgressKind,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl PendingLogEntry {
    pub(crate) fn target(&self) -> Result<EgressTarget> {
        EgressTarget::new(self.kind, self.host.clone(), self.port)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AllowedItem {
    pub(crate) target: String,
    pub(crate) kind: EgressKind,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) connector_endpoint: Option<String>,
}

impl AllowedItem {
    pub(crate) fn from_target(target: &EgressTarget, connector_endpoint: Option<String>) -> Self {
        Self {
            target: target.to_string(),
            kind: target.kind,
            host: target.host.clone(),
            port: target.port,
            connector_endpoint,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PendingItem {
    pub(crate) target: String,
    pub(crate) kind: EgressKind,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) connector_endpoint: Option<String>,
    pub(crate) last_seen_epoch_nanos: String,
    #[serde(skip)]
    pub(crate) epoch: u64,
}

impl PendingItem {
    pub(crate) fn from_target(
        target: &EgressTarget,
        connector_endpoint: Option<String>,
        last_seen_epoch_nanos: String,
        epoch: u64,
    ) -> Self {
        Self {
            target: target.to_string(),
            kind: target.kind,
            host: target.host.clone(),
            port: target.port,
            connector_endpoint,
            last_seen_epoch_nanos,
            epoch,
        }
    }
}

pub(crate) fn parse_target_spec(value: &str) -> Result<EgressTarget> {
    let value = value.trim();
    if value.is_empty() {
        bail!("invalid destination");
    }
    if let Some((scheme, remainder)) = value.split_once("://") {
        let kind = parse_kind(scheme)?;
        let authority = trim_authority(remainder);
        let (host, port) = split_host_port(authority, kind.default_port())?;
        return EgressTarget::new(kind, host, port);
    }

    let (host, port) = split_host_port(value, EgressKind::Https.default_port())?;
    EgressTarget::new(EgressKind::Https, host, port)
}

pub(crate) fn read_allowed_targets_file(path: &Path) -> Result<Vec<EgressTarget>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut targets = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(parse_target_spec)
        .collect::<Result<Vec<_>>>()?;
    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub(crate) fn read_connector_mappings_file(path: &Path) -> Result<Vec<ConnectorMapping>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut mappings: Vec<ConnectorMapping> = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    sort_and_dedup_connector_mappings(&mut mappings);
    Ok(mappings)
}

pub(crate) fn ensure_connector_mapping(
    mappings: &mut Vec<ConnectorMapping>,
    target: &EgressTarget,
) -> Result<ConnectorMapping> {
    if !target.uses_connector() {
        bail!("destination does not use a broker connector");
    }
    sort_and_dedup_connector_mappings(mappings);
    if let Some(existing) = mappings.iter().find(|mapping| mapping.target == *target) {
        return Ok(existing.clone());
    }
    let used_ports = mappings
        .iter()
        .map(|mapping| mapping.listen_port)
        .collect::<BTreeSet<_>>();
    let listen_port = (CONNECTOR_PORT_START..=CONNECTOR_PORT_END)
        .find(|port| !used_ports.contains(port))
        .context("no connector ports available")?;
    let mapping = ConnectorMapping {
        target: target.clone(),
        listen_port,
    };
    mappings.push(mapping.clone());
    sort_and_dedup_connector_mappings(mappings);
    Ok(mapping)
}

pub(crate) fn remove_connector_mapping(
    mappings: &mut Vec<ConnectorMapping>,
    target: &EgressTarget,
) -> bool {
    let before = mappings.len();
    mappings.retain(|mapping| mapping.target != *target);
    sort_and_dedup_connector_mappings(mappings);
    mappings.len() != before
}

pub(crate) fn serialize_connector_mappings(mappings: &[ConnectorMapping]) -> Result<Vec<u8>> {
    let mut normalized = mappings.to_vec();
    sort_and_dedup_connector_mappings(&mut normalized);
    let mut bytes =
        serde_json::to_vec_pretty(&normalized).context("failed to serialize connector mappings")?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn serialize_target_set(targets: impl IntoIterator<Item = EgressTarget>) -> String {
    targets
        .into_iter()
        .map(|item| format!("{item}\n"))
        .collect::<String>()
}

pub(crate) fn connector_endpoint(listen_port: u16) -> String {
    format!("{BROKER_INTERNAL_HOST}:{listen_port}")
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
    if let Some((head, _)) = value.split_once('?') {
        value = head.to_string();
    }
    if let Some((head, _)) = value.split_once('#') {
        value = head.to_string();
    }
    if let Some(stripped) = value.strip_prefix('[') {
        let (host, _) = stripped.split_once(']').context("invalid hostname")?;
        value = host.to_string();
    } else if value.matches(':').count() == 1 {
        let (host, port) = value.split_once(':').context("invalid hostname")?;
        if !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
            value = host.to_string();
        }
    }
    value = value.trim_matches('.').to_string();
    if value.is_empty() {
        bail!("invalid hostname");
    }
    validate_normalized_host(&value)?;
    Ok(value)
}

pub(crate) fn split_host_port(value: &str, default_port: u16) -> Result<(String, u16)> {
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
    if default_port == 0 {
        bail!("port is required");
    }
    Ok((normalize_host(trimmed)?, default_port))
}

fn parse_kind(value: &str) -> Result<EgressKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "http" => Ok(EgressKind::Http),
        "https" => Ok(EgressKind::Https),
        "tcp" => Ok(EgressKind::Tcp),
        "ssh" => Ok(EgressKind::Ssh),
        "mcp" => Ok(EgressKind::Mcp),
        _ => bail!("unsupported destination kind"),
    }
}

fn sort_and_dedup_connector_mappings(mappings: &mut Vec<ConnectorMapping>) {
    mappings.sort_by(|a, b| {
        a.target
            .cmp(&b.target)
            .then_with(|| a.listen_port.cmp(&b.listen_port))
    });
    mappings.dedup_by(|a, b| a.target == b.target);
}

fn trim_authority(value: &str) -> &str {
    value.split(['/', '?', '#']).next().unwrap_or(value)
}

fn host_for_uri(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn validate_normalized_host(value: &str) -> Result<()> {
    if value.chars().any(|ch| ch.is_ascii_whitespace()) {
        bail!("invalid hostname");
    }
    if value.contains(':') {
        if value
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() || ch == ':' || ch == '.')
            && value.contains(':')
        {
            return Ok(());
        }
        bail!("invalid hostname");
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '-')
    {
        return Ok(());
    }
    bail!("invalid hostname");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_target_defaults_port() {
        let target = parse_target_spec("https://Objects.GitHubusercontent.com/foo").unwrap();
        assert_eq!(
            target.to_string(),
            "https://objects.githubusercontent.com:443"
        );
    }

    #[test]
    fn parse_target_without_scheme_defaults_to_https() {
        let target = parse_target_spec("github.com").unwrap();
        assert_eq!(target.to_string(), "https://github.com:443");
    }

    #[test]
    fn parse_tcp_target_requires_port() {
        assert!(parse_target_spec("tcp://db.example").is_err());
    }

    #[test]
    fn connector_endpoint_uses_broker_host() {
        assert_eq!(connector_endpoint(46001), "llm-box-broker:46001");
    }
}

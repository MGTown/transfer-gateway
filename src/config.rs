use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    ip2region::IpLocation,
    language::{DEFAULT_LANGUAGE_DIRECTORY, DEFAULT_LOCALE},
    protocol,
    security::Security,
};

pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.example.toml");
pub const DEFAULT_VHOST_TEMPLATES: &[(&str, &str)] = &[
    ("alpha", include_str!("../vhosts/alpha.toml")),
    ("beta", include_str!("../vhosts/beta.toml")),
];

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub ip2region: Ip2RegionConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    #[serde(default)]
    pub vhosts: Option<VhostsConfig>,
    #[serde(default)]
    pub language: LanguageConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("unable to read configuration file {}", path.display()))?;
        let mut config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid TOML in {}", path.display()))?;
        if let Some(vhosts) = config.vhosts.as_mut() {
            vhosts.load(path)?;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn resolve_paths(&mut self, config_path: &Path) {
        self.ip2region.resolve_paths(config_path);
        self.security.resolve_paths(config_path);
    }

    fn validate(&self) -> Result<()> {
        self.server.validate()?;
        self.ip2region.validate()?;
        self.security.validate()?;
        match (&self.routing, &self.vhosts) {
            (Some(_), Some(_)) => bail!("configure either routing or vhosts, not both"),
            (Some(routing), None) => routing.validate()?,
            (None, Some(vhosts)) => vhosts.validate()?,
            (None, None) => bail!("routing or vhosts must be configured"),
        }
        self.language.validate()?;
        Ok(())
    }

    pub fn select_route_with_host_context(
        &self,
        location: &IpLocation,
        player: Option<&str>,
        request_host: Option<&str>,
        security: &Security,
        balance_key: &str,
    ) -> Option<(String, LineTarget)> {
        if let Some(vhosts) = &self.vhosts {
            return vhosts.select_route_with_context(
                location,
                player,
                request_host,
                security,
                balance_key,
            );
        }

        self.routing.as_ref()?.select_route_with_host_context(
            location,
            player,
            request_host,
            security,
            balance_key,
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_login_timeout_ms")]
    pub login_timeout_ms: u64,
    #[serde(default = "default_max_frame_length")]
    pub max_frame_length: usize,
    #[serde(default = "default_supported_protocols")]
    pub supported_protocols: Vec<i32>,
    #[serde(default = "default_status_version_name")]
    pub status_version_name: String,
    #[serde(default = "default_status_protocol")]
    pub status_protocol: i32,
    #[serde(default = "default_max_players")]
    pub max_players: u32,
    #[serde(default = "default_motd")]
    pub motd: String,
    #[serde(default)]
    pub strict_error_handling: bool,
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        if self.bind.trim().is_empty() {
            bail!("server.bind must not be empty");
        }
        if self.max_connections == 0 {
            bail!("server.max_connections must be greater than zero");
        }
        if self.login_timeout_ms == 0 {
            bail!("server.login_timeout_ms must be greater than zero");
        }
        if !(1024..=16 * 1024 * 1024).contains(&self.max_frame_length) {
            bail!("server.max_frame_length must be between 1024 and 16777216");
        }
        for version in &self.supported_protocols {
            if !protocol::is_supported_protocol(*version) {
                bail!(
                    "server.supported_protocols contains unsupported protocol {}; supported range is {}",
                    version,
                    protocol::SUPPORTED_VERSION_RANGE
                );
            }
        }
        if self.status_protocol < 0 {
            bail!("server.status_protocol must not be negative");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ip2RegionConfig {
    #[serde(default = "default_ip2region_v4_db")]
    pub v4_db: Option<PathBuf>,
    #[serde(default = "default_ip2region_v6_db")]
    pub v6_db: Option<PathBuf>,
    #[serde(default = "default_ip2region_auto_download")]
    pub auto_download: bool,
    #[serde(default = "default_ip2region_auto_update")]
    pub auto_update: bool,
    #[serde(
        default = "default_ip2region_update_interval_secs",
        alias = "update_delay_secs",
        alias = "update_delay",
        alias = "delay_secs",
        alias = "delay"
    )]
    pub update_interval_secs: u64,
    #[serde(default = "default_ip2region_download_base_url")]
    pub download_base_url: String,
}

impl Default for Ip2RegionConfig {
    fn default() -> Self {
        Self {
            v4_db: default_ip2region_v4_db(),
            v6_db: default_ip2region_v6_db(),
            auto_download: default_ip2region_auto_download(),
            auto_update: default_ip2region_auto_update(),
            update_interval_secs: default_ip2region_update_interval_secs(),
            download_base_url: default_ip2region_download_base_url(),
        }
    }
}

impl Ip2RegionConfig {
    fn validate(&self) -> Result<()> {
        if self.auto_download || self.auto_update {
            let url = reqwest::Url::parse(&self.download_base_url).with_context(|| {
                format!(
                    "ip2region.download_base_url is not a valid URL: {}",
                    self.download_base_url
                )
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!(
                    "ip2region.download_base_url must use http or https, got {}",
                    url.scheme()
                );
            }
        }
        if self.auto_update && self.update_interval_secs == 0 {
            bail!("ip2region.update_interval_secs must be greater than zero");
        }
        Ok(())
    }

    pub fn resolve_paths(&mut self, config_path: &Path) {
        self.v4_db = self
            .v4_db
            .take()
            .map(|path| resolve_ip2region_path(config_path, &path));
        self.v6_db = self
            .v6_db
            .take()
            .map(|path| resolve_ip2region_path(config_path, &path));
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_security_enabled")]
    pub enabled: bool,
    #[serde(default = "default_security_block_vpn")]
    pub block_vpn: bool,
    #[serde(default = "default_security_block_tor")]
    pub block_tor: bool,
    #[serde(default = "default_security_block_spam")]
    pub block_spam: bool,
    #[serde(default = "default_security_auto_download")]
    pub auto_download: bool,
    #[serde(default = "default_security_auto_update")]
    pub auto_update: bool,
    #[serde(
        default = "default_security_update_interval_secs",
        alias = "update_delay_secs",
        alias = "update_delay",
        alias = "delay_secs",
        alias = "delay"
    )]
    pub update_interval_secs: u64,
    #[serde(default = "default_tor_exit_list")]
    pub tor_exit_list: Option<PathBuf>,
    #[serde(default = "default_tor_exit_list_url")]
    pub tor_exit_list_url: String,
    #[serde(default = "default_spam_list")]
    pub spam_list: Option<PathBuf>,
    #[serde(default = "default_spam_list_url")]
    pub spam_list_url: String,
    #[serde(default = "default_vpn_ipv4_list")]
    pub vpn_ipv4_list: Option<PathBuf>,
    #[serde(default = "default_vpn_ipv4_list_url")]
    pub vpn_ipv4_list_url: String,
    #[serde(default = "default_vpn_ipv6_list")]
    pub vpn_ipv6_list: Option<PathBuf>,
    #[serde(default = "default_vpn_ipv6_list_url")]
    pub vpn_ipv6_list_url: String,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub vpn_isp_contains: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enabled: default_security_enabled(),
            block_vpn: default_security_block_vpn(),
            block_tor: default_security_block_tor(),
            block_spam: default_security_block_spam(),
            auto_download: default_security_auto_download(),
            auto_update: default_security_auto_update(),
            update_interval_secs: default_security_update_interval_secs(),
            tor_exit_list: default_tor_exit_list(),
            tor_exit_list_url: default_tor_exit_list_url(),
            spam_list: default_spam_list(),
            spam_list_url: default_spam_list_url(),
            vpn_ipv4_list: default_vpn_ipv4_list(),
            vpn_ipv4_list_url: default_vpn_ipv4_list_url(),
            vpn_ipv6_list: default_vpn_ipv6_list(),
            vpn_ipv6_list_url: default_vpn_ipv6_list_url(),
            allowlist: Vec::new(),
            vpn_isp_contains: Vec::new(),
        }
    }
}

impl SecurityConfig {
    fn validate(&self) -> Result<()> {
        if (self.auto_download || self.auto_update) && self.enabled {
            if self.block_tor && configured_path(self.tor_exit_list.as_deref()) {
                validate_http_url("security.tor_exit_list_url", &self.tor_exit_list_url)?;
            }
            if self.block_spam && configured_path(self.spam_list.as_deref()) {
                validate_http_url("security.spam_list_url", &self.spam_list_url)?;
            }
            if self.block_vpn {
                if configured_path(self.vpn_ipv4_list.as_deref()) {
                    validate_http_url("security.vpn_ipv4_list_url", &self.vpn_ipv4_list_url)?;
                }
                if configured_path(self.vpn_ipv6_list.as_deref()) {
                    validate_http_url("security.vpn_ipv6_list_url", &self.vpn_ipv6_list_url)?;
                }
            }
        }
        if self.auto_update && self.update_interval_secs == 0 {
            bail!("security.update_interval_secs must be greater than zero");
        }
        Ok(())
    }

    pub fn resolve_paths(&mut self, config_path: &Path) {
        self.tor_exit_list = self
            .tor_exit_list
            .take()
            .map(|path| resolve_security_path(config_path, &path));
        self.spam_list = self
            .spam_list
            .take()
            .map(|path| resolve_security_path(config_path, &path));
        self.vpn_ipv4_list = self
            .vpn_ipv4_list
            .take()
            .map(|path| resolve_security_path(config_path, &path));
        self.vpn_ipv6_list = self
            .vpn_ipv6_list
            .take()
            .map(|path| resolve_security_path(config_path, &path));
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LanguageConfig {
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_language_directory")]
    pub directory: PathBuf,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            locale: default_locale(),
            directory: default_language_directory(),
        }
    }
}

impl LanguageConfig {
    fn validate(&self) -> Result<()> {
        if self.locale.is_empty()
            || !self
                .locale
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
        {
            bail!("language.locale must contain only ASCII letters, digits, '.', '-' or '_'");
        }
        if self.directory.as_os_str().is_empty() {
            bail!("language.directory must not be empty");
        }
        Ok(())
    }

    pub fn file_path(&self, config_path: &Path) -> PathBuf {
        resolve_relative_path(config_path, &self.directory).join(format!("{}.toml", self.locale))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_file")]
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file: default_log_file(),
        }
    }
}

impl LoggingConfig {
    pub fn file_path(&self, config_path: &Path) -> Option<PathBuf> {
        self.file
            .as_deref()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| resolve_relative_path(config_path, path))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VhostsConfig {
    #[serde(default)]
    pub default: String,
    #[serde(flatten, default)]
    pub files: BTreeMap<String, PathBuf>,
    #[serde(skip, default)]
    routes: BTreeMap<String, RoutingConfig>,
}

impl VhostsConfig {
    fn load(&mut self, config_path: &Path) -> Result<()> {
        self.routes.clear();
        for (name, configured_path) in &self.files {
            if configured_path.as_os_str().is_empty() {
                bail!("vhosts.{name} must contain a configuration file path");
            }
            let path = resolve_relative_path(config_path, configured_path);
            let routing = load_vhost_routing(&path).with_context(|| {
                format!("unable to load vhost '{name}' from {}", path.display())
            })?;
            self.routes.insert(name.clone(), routing);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.files.is_empty() {
            bail!("vhosts must contain at least one host mapping");
        }
        if self.default.trim().is_empty() {
            bail!("vhosts.default must not be empty");
        }

        let default_exists = self
            .files
            .keys()
            .any(|name| normalize_host(name).eq_ignore_ascii_case(normalize_host(&self.default)));
        if !default_exists {
            bail!("vhosts.default '{}' is not present in vhosts", self.default);
        }

        let mut normalized_names = Vec::with_capacity(self.files.len());
        for name in self.files.keys() {
            let normalized_name = normalize_host(name);
            if normalized_name.is_empty() {
                bail!("vhosts contains an empty host name");
            }
            if normalized_name.len() > 255 {
                bail!("vhosts host name '{}' must be at most 255 bytes", name);
            }
            if normalized_names
                .iter()
                .any(|previous: &&str| previous.eq_ignore_ascii_case(normalized_name))
            {
                bail!("vhosts host name '{}' is duplicated", name);
            }
            normalized_names.push(normalized_name);

            let routing = self.routes.get(name).ok_or_else(|| {
                anyhow::anyhow!("vhosts.{name} has not been loaded from its configuration file")
            })?;
            routing
                .validate()
                .with_context(|| format!("invalid routing configuration for vhost '{name}'"))?;
        }
        Ok(())
    }

    fn select_route_with_context(
        &self,
        location: &IpLocation,
        player: Option<&str>,
        request_host: Option<&str>,
        security: &Security,
        balance_key: &str,
    ) -> Option<(String, LineTarget)> {
        let (vhost_name, routing) = self.select_routing(request_host)?;
        let (line_name, target) = routing.select_route_with_host_context(
            location,
            player,
            request_host,
            security,
            balance_key,
        )?;
        Some((format!("{vhost_name}/{line_name}"), target))
    }

    fn select_routing(&self, request_host: Option<&str>) -> Option<(&str, &RoutingConfig)> {
        if let Some(request_host) = request_host {
            let request_host = normalize_host(request_host);
            if let Some((name, routing)) = self
                .routes
                .iter()
                .find(|(name, _)| normalize_host(name).eq_ignore_ascii_case(request_host))
            {
                return Some((name.as_str(), routing));
            }
        }

        self.routes
            .iter()
            .find(|(name, _)| {
                normalize_host(name).eq_ignore_ascii_case(normalize_host(&self.default))
            })
            .map(|(name, routing)| (name.as_str(), routing))
    }
}

fn load_vhost_routing(path: &Path) -> Result<RoutingConfig> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("unable to read vhost configuration file {}", path.display()))?;
    let mut document: toml::Value = toml::from_str(&source).with_context(|| {
        format!(
            "invalid TOML in vhost configuration file {}",
            path.display()
        )
    })?;

    if let Some(table) = document.as_table_mut()
        && let Some(routing) = table.remove("routing")
    {
        document = routing;
    }

    let is_single_target = document.as_table().is_some_and(|table| {
        table.contains_key("host")
            && !table.contains_key("default_line")
            && !table.contains_key("lines")
            && !table.contains_key("rules")
            && !table.contains_key("group")
            && !table.contains_key("groups")
    });
    if is_single_target {
        let target: LineTarget = document
            .try_into()
            .context("invalid single-target vhost configuration")?;
        return Ok(RoutingConfig::from_single_target(target));
    }

    document
        .try_into()
        .context("invalid routing configuration in vhost file")
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_line")]
    pub default_line: String,
    #[serde(default)]
    pub lines: BTreeMap<String, LineTarget>,
    #[serde(default)]
    pub rules: Vec<RouteRule>,
    #[serde(default, alias = "groups")]
    pub group: Vec<RoutingGroup>,
}

impl RoutingConfig {
    fn from_single_target(target: LineTarget) -> Self {
        let mut lines = BTreeMap::new();
        lines.insert("default".to_owned(), target);
        Self {
            default_line: "default".to_owned(),
            lines,
            rules: Vec::new(),
            group: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.lines.is_empty() && self.group.is_empty() {
            bail!("routing.lines or routing.group must contain at least one target");
        }
        if !self.default_line.is_empty() && !self.has_target(&self.default_line) {
            bail!(
                "routing.default_line '{}' is not present in routing.lines or routing.group",
                self.default_line
            );
        }
        if self.default_line.is_empty() && self.rules.is_empty() {
            bail!("routing.default_line must not be empty when routing.rules is empty");
        }

        for (name, target) in &self.lines {
            target.validate(name)?;
        }
        for (index, group) in self.group.iter().enumerate() {
            group.validate(index)?;
            if self.lines.contains_key(&group.group_name)
                || self.group.iter().enumerate().any(|(other_index, other)| {
                    other_index < index && other.group_name == group.group_name
                })
            {
                bail!(
                    "routing.group[{index}].group_name '{}' is duplicated",
                    group.group_name
                );
            }
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let has_line = !rule.line.trim().is_empty();
            let has_group = rule
                .group
                .as_deref()
                .is_some_and(|group| !group.trim().is_empty());
            if has_line == has_group {
                bail!("routing.rules[{index}] must define exactly one of line or group");
            }
            if has_line && !self.has_target(&rule.line) {
                bail!(
                    "routing.rules[{index}].line '{}' is not present in routing.lines or routing.group",
                    rule.line
                );
            }
            if has_group
                && !self
                    .group
                    .iter()
                    .any(|group| Some(group.group_name.as_str()) == rule.group.as_deref())
            {
                bail!(
                    "routing.rules[{index}].group '{}' is not present in routing.group",
                    rule.group.as_deref().unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    pub fn select_route<'a>(&'a self, location: &IpLocation) -> Option<(&'a str, &'a LineTarget)> {
        self.select_route_for_host(location, None)
    }

    pub fn select_route_for_host<'a>(
        &'a self,
        location: &IpLocation,
        request_host: Option<&str>,
    ) -> Option<(&'a str, &'a LineTarget)> {
        let line_name = self
            .select_matching_rule(location, None, request_host, None)
            .and_then(|rule| rule.line_target_name(self))
            .unwrap_or(self.default_line.as_str());
        self.lines
            .get_key_value(line_name)
            .map(|(name, target)| (name.as_str(), target))
    }

    pub fn select_route_with_context(
        &self,
        location: &IpLocation,
        player: Option<&str>,
        security: &Security,
        balance_key: &str,
    ) -> Option<(String, LineTarget)> {
        self.select_route_with_host_context(location, player, None, security, balance_key)
    }

    pub fn select_route_with_host_context(
        &self,
        location: &IpLocation,
        player: Option<&str>,
        request_host: Option<&str>,
        security: &Security,
        balance_key: &str,
    ) -> Option<(String, LineTarget)> {
        if let Some(rule) =
            self.select_matching_rule(location, player, request_host, Some(security))
        {
            if let Some(line_name) = rule.line_target_name(self)
                && let Some(target) = self.lines.get(line_name)
            {
                return Some((line_name.to_owned(), target.clone()));
            }
            if let Some(group_name) = rule.group_target_name()
                && let Some(group) = self
                    .group
                    .iter()
                    .find(|group| group.group_name == group_name)
            {
                return Some((group_name.to_owned(), group.select_target(balance_key)));
            }
        }

        self.select_target(&self.default_line, balance_key)
    }

    fn select_matching_rule<'a>(
        &'a self,
        location: &IpLocation,
        player: Option<&str>,
        request_host: Option<&str>,
        security: Option<&Security>,
    ) -> Option<&'a RouteRule> {
        let mut selected: Option<(i32, usize, &RouteRule)> = None;

        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.matches(location, player, request_host, security) {
                continue;
            }

            let priority = rule.effective_priority(self);
            let should_replace = selected
                .as_ref()
                .map(|(selected_priority, _, _)| priority > *selected_priority)
                .unwrap_or(true);
            if should_replace {
                selected = Some((priority, index, rule));
            }
        }

        selected.map(|(_, _, rule)| rule)
    }

    fn has_target(&self, name: &str) -> bool {
        self.lines.contains_key(name) || self.group.iter().any(|group| group.group_name == name)
    }

    fn select_target(&self, name: &str, balance_key: &str) -> Option<(String, LineTarget)> {
        if let Some(target) = self.lines.get(name) {
            return Some((name.to_owned(), target.clone()));
        }
        self.group
            .iter()
            .find(|group| group.group_name == name)
            .map(|group| (name.to_owned(), group.select_target(balance_key)))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LineTarget {
    pub host: String,
    #[serde(default = "default_line_port")]
    pub port: u16,
    #[serde(
        default = "default_resolve_srv",
        alias = "minecraft_srv",
        alias = "srv"
    )]
    pub resolve_srv: bool,
}

impl LineTarget {
    fn validate(&self, name: &str) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("routing.lines.{name}.host must not be empty");
        }
        if self.host.len() > 255 {
            bail!("routing.lines.{name}.host must be at most 255 bytes");
        }
        if self.port == 0 {
            bail!("routing.lines.{name}.port must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingGroup {
    #[serde(default)]
    pub priority: i32,
    pub group_name: String,
    #[serde(default = "default_group_mode")]
    pub mode: String,
    #[serde(default = "default_group_port")]
    pub port: u16,
    #[serde(
        default = "default_resolve_srv",
        alias = "minecraft_srv",
        alias = "srv"
    )]
    pub resolve_srv: bool,
    pub hosts: Vec<String>,
    #[serde(skip, default = "default_group_counter")]
    counter: Arc<AtomicUsize>,
}

impl RoutingGroup {
    fn validate(&self, index: usize) -> Result<()> {
        if self.group_name.trim().is_empty() {
            bail!("routing.group[{index}].group_name must not be empty");
        }
        if self.hosts.is_empty() {
            bail!("routing.group[{index}].hosts must contain at least one host");
        }
        if self.port == 0 {
            bail!("routing.group[{index}].port must be greater than zero");
        }
        if parse_group_mode(&self.mode).is_none() {
            bail!(
                "routing.group[{index}].mode '{}' is unsupported; use round_robin, random or ip_hash",
                self.mode
            );
        }
        for (host_index, host) in self.hosts.iter().enumerate() {
            if host.trim().is_empty() {
                bail!("routing.group[{index}].hosts[{host_index}] must not be empty");
            }
            if host.len() > 255 {
                bail!("routing.group[{index}].hosts[{host_index}] must be at most 255 bytes");
            }
        }
        Ok(())
    }

    fn select_target(&self, balance_key: &str) -> LineTarget {
        let index = match parse_group_mode(&self.mode).unwrap_or(GroupMode::RoundRobin) {
            GroupMode::RoundRobin => {
                self.counter.fetch_add(1, Ordering::Relaxed) % self.hosts.len()
            }
            GroupMode::Random => {
                let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
                pseudo_random_index(sequence, self.hosts.len())
            }
            GroupMode::IpHash => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                balance_key.hash(&mut hasher);
                (hasher.finish() as usize) % self.hosts.len()
            }
        };
        LineTarget {
            host: self.hosts[index].clone(),
            port: self.port,
            resolve_srv: self.resolve_srv,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RouteRule {
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub line: String,
    #[serde(default, alias = "group_name")]
    pub group: Option<String>,
    #[serde(default, alias = "hostnames", alias = "domains")]
    pub hosts: Vec<String>,
    #[serde(default, alias = "not_hostnames", alias = "not_domains")]
    pub not_hosts: Vec<String>,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default, alias = "subdivisions")]
    pub provinces: Vec<String>,
    #[serde(default, alias = "not_subdivisions")]
    pub not_provinces: Vec<String>,
    #[serde(default)]
    pub cities: Vec<String>,
    #[serde(default)]
    pub not_countries: Vec<String>,
    #[serde(default)]
    pub not_cities: Vec<String>,
    #[serde(default, alias = "operator_contains")]
    pub isp_contains: Vec<String>,
    #[serde(default, alias = "not_operator_contains")]
    pub not_isp_contains: Vec<String>,
    #[serde(default)]
    pub players: Vec<String>,
    #[serde(default)]
    pub not_players: Vec<String>,
    #[serde(default)]
    pub vpn: Option<bool>,
    #[serde(default)]
    pub not_vpn: bool,
    #[serde(default)]
    pub spam: Option<bool>,
    #[serde(default)]
    pub not_spam: bool,
    #[serde(default)]
    pub tor: Option<bool>,
    #[serde(default)]
    pub not_tor: bool,
}

impl RouteRule {
    fn matches(
        &self,
        location: &IpLocation,
        player: Option<&str>,
        request_host: Option<&str>,
        security: Option<&Security>,
    ) -> bool {
        matches_host_list(&self.hosts, &self.not_hosts, request_host)
            && matches_text_list(
                &self.countries,
                &self.not_countries,
                location.country_code.as_deref(),
            )
            && matches_text_list(
                &self.provinces,
                &self.not_provinces,
                location.province.as_deref(),
            )
            && matches_text_list(&self.cities, &self.not_cities, location.city.as_deref())
            && matches_contains_list(
                &self.isp_contains,
                &self.not_isp_contains,
                location.isp.as_deref(),
            )
            && self.matches_player(player)
            && self.matches_security(location, security)
    }

    fn matches_player(&self, player: Option<&str>) -> bool {
        if self.players.is_empty() && self.not_players.is_empty() {
            return true;
        }
        let Some(player) = player else { return false };
        matches_text_list(&self.players, &self.not_players, Some(player))
    }

    fn matches_security(&self, location: &IpLocation, security: Option<&Security>) -> bool {
        if self.vpn.is_none()
            && !self.not_vpn
            && self.spam.is_none()
            && !self.not_spam
            && self.tor.is_none()
            && !self.not_tor
        {
            return true;
        }
        let Some(security) = security else {
            return false;
        };
        matches_bool(self.vpn, self.not_vpn, security.matches_vpn(location))
            && matches_bool(self.spam, self.not_spam, security.matches_spam(location))
            && matches_bool(self.tor, self.not_tor, security.matches_tor(location))
    }

    fn line_target_name<'a>(&'a self, routing: &'a RoutingConfig) -> Option<&'a str> {
        (!self.line.trim().is_empty() && routing.lines.contains_key(&self.line))
            .then_some(self.line.as_str())
    }

    fn group_target_name(&self) -> Option<&str> {
        self.group
            .as_deref()
            .filter(|group| !group.trim().is_empty())
            .or_else(|| (!self.line.trim().is_empty()).then_some(self.line.as_str()))
    }

    fn effective_priority(&self, routing: &RoutingConfig) -> i32 {
        if self.priority != 0 {
            return self.priority;
        }
        self.group_target_name()
            .and_then(|target| {
                routing
                    .group
                    .iter()
                    .find(|group| group.group_name == target)
                    .map(|group| group.priority)
            })
            .unwrap_or(self.priority)
    }
}

fn matches_text_list(positive: &[String], negative: &[String], actual: Option<&str>) -> bool {
    let has_positive = positive
        .iter()
        .any(|expected| bang_negation(expected).is_none());
    let positive_matches = !has_positive
        || actual.is_some_and(|actual| {
            positive.iter().any(|expected| {
                bang_negation(expected).is_none() && expected.eq_ignore_ascii_case(actual)
            })
        });
    let negative_matches = actual.is_some_and(|actual| {
        negative
            .iter()
            .any(|expected| matcher_value(expected).eq_ignore_ascii_case(actual))
            || positive
                .iter()
                .filter_map(|expected| bang_negation(expected))
                .any(|expected| expected.eq_ignore_ascii_case(actual))
    });
    positive_matches && !negative_matches
}

fn matches_host_list(positive: &[String], negative: &[String], actual: Option<&str>) -> bool {
    let actual = actual.map(normalize_host);
    let has_positive = positive
        .iter()
        .any(|expected| bang_negation(expected).is_none());
    let positive_matches = !has_positive
        || actual.is_some_and(|actual| {
            positive.iter().any(|expected| {
                bang_negation(expected).is_none()
                    && normalize_host(expected).eq_ignore_ascii_case(actual)
            })
        });
    let negative_matches = actual.is_some_and(|actual| {
        negative
            .iter()
            .any(|expected| normalize_host(matcher_value(expected)).eq_ignore_ascii_case(actual))
            || positive
                .iter()
                .filter_map(|expected| bang_negation(expected))
                .any(|expected| normalize_host(expected).eq_ignore_ascii_case(actual))
    });
    positive_matches && !negative_matches
}

fn normalize_host(host: &str) -> &str {
    host.trim_end_matches('.')
}

fn matches_contains_list(positive: &[String], negative: &[String], actual: Option<&str>) -> bool {
    let actual_lower = actual.map(str::to_lowercase);
    let has_positive = positive
        .iter()
        .any(|expected| bang_negation(expected).is_none());
    let positive_matches = !has_positive
        || actual_lower.as_deref().is_some_and(|actual| {
            positive.iter().any(|expected| {
                bang_negation(expected).is_none()
                    && actual.contains(expected.to_lowercase().as_str())
            })
        });
    let negative_matches = actual_lower.as_deref().is_some_and(|actual| {
        negative
            .iter()
            .any(|expected| actual.contains(matcher_value(expected).to_lowercase().as_str()))
            || positive
                .iter()
                .filter_map(|expected| bang_negation(expected))
                .any(|expected| actual.contains(expected.to_lowercase().as_str()))
    });
    positive_matches && !negative_matches
}

fn bang_negation(value: &str) -> Option<&str> {
    value.strip_prefix('!').filter(|value| !value.is_empty())
}

fn matcher_value(value: &str) -> &str {
    bang_negation(value).unwrap_or(value)
}

fn matches_bool(expected: Option<bool>, negated: bool, actual: bool) -> bool {
    expected.is_none_or(|expected| expected == actual) && (!negated || !actual)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupMode {
    RoundRobin,
    Random,
    IpHash,
}

fn parse_group_mode(mode: &str) -> Option<GroupMode> {
    let normalized: String = mode
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '_' && *character != '-')
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "roundrobin" | "loadbalance" | "loadblance" => Some(GroupMode::RoundRobin),
        "random" => Some(GroupMode::Random),
        "iphash" | "hash" | "consistenthash" => Some(GroupMode::IpHash),
        _ => None,
    }
}

fn pseudo_random_index(sequence: usize, length: usize) -> usize {
    let mut value = (sequence as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    value ^= value >> 33;
    value = value.wrapping_mul(1_099_511_628_211u64);
    value ^= value >> 29;
    (value as usize) % length
}

fn default_bind() -> String {
    "0.0.0.0:25565".to_owned()
}

fn default_max_connections() -> usize {
    4096
}

fn default_login_timeout_ms() -> u64 {
    10_000
}

fn default_max_frame_length() -> usize {
    2 * 1024 * 1024
}

fn default_supported_protocols() -> Vec<i32> {
    Vec::new()
}

fn default_status_version_name() -> String {
    protocol::SUPPORTED_VERSION_RANGE.to_owned()
}

fn default_status_protocol() -> i32 {
    protocol::LATEST_SNAPSHOT_PROTOCOL
}

fn default_max_players() -> u32 {
    100
}

fn default_motd() -> String {
    "Transfer Gateway".to_owned()
}

fn default_ip2region_v4_db() -> Option<PathBuf> {
    Some(PathBuf::from("./data/ip2region_v4.xdb"))
}

fn default_ip2region_v6_db() -> Option<PathBuf> {
    Some(PathBuf::from("./data/ip2region_v6.xdb"))
}

fn default_ip2region_auto_download() -> bool {
    true
}

fn default_ip2region_auto_update() -> bool {
    false
}

fn default_ip2region_update_interval_secs() -> u64 {
    24 * 60 * 60
}

fn default_ip2region_download_base_url() -> String {
    "https://raw.githubusercontent.com/lionsoul2014/ip2region/master/data".to_owned()
}

fn default_security_enabled() -> bool {
    true
}

fn default_security_block_vpn() -> bool {
    true
}

fn default_security_block_tor() -> bool {
    true
}

fn default_security_block_spam() -> bool {
    true
}

fn default_security_auto_download() -> bool {
    true
}

fn default_security_auto_update() -> bool {
    false
}

fn default_security_update_interval_secs() -> u64 {
    24 * 60 * 60
}

fn default_tor_exit_list() -> Option<PathBuf> {
    Some(PathBuf::from("./data/tor-exit-list.txt"))
}

fn default_tor_exit_list_url() -> String {
    "https://check.torproject.org/torbulkexitlist".to_owned()
}

fn default_spam_list() -> Option<PathBuf> {
    Some(PathBuf::from("./data/spam-ip-list.txt"))
}

fn default_spam_list_url() -> String {
    "https://blackip.ustc.edu.cn/list.php?txt".to_owned()
}

fn default_vpn_ipv4_list() -> Option<PathBuf> {
    Some(PathBuf::from("./data/vpn-ipv4.txt"))
}

fn default_vpn_ipv4_list_url() -> String {
    "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv4.txt".to_owned()
}

fn default_vpn_ipv6_list() -> Option<PathBuf> {
    Some(PathBuf::from("./data/vpn-ipv6.txt"))
}

fn default_vpn_ipv6_list_url() -> String {
    "https://raw.githubusercontent.com/X4BNet/lists_vpn/main/output/vpn/ipv6.txt".to_owned()
}

fn default_line() -> String {
    "default".to_owned()
}

fn default_group_mode() -> String {
    "round_robin".to_owned()
}

fn default_group_port() -> u16 {
    25565
}

fn default_line_port() -> u16 {
    25565
}

fn default_resolve_srv() -> bool {
    false
}

fn default_group_counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn default_locale() -> String {
    DEFAULT_LOCALE.to_owned()
}

fn default_language_directory() -> PathBuf {
    PathBuf::from(DEFAULT_LANGUAGE_DIRECTORY)
}

fn default_log_file() -> Option<PathBuf> {
    Some(PathBuf::from("./logs/gateway.log"))
}

fn resolve_relative_path(config_path: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }

    let base = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    base.join(path)
}

fn resolve_ip2region_path(config_path: &Path, path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        path.to_owned()
    } else {
        resolve_relative_path(config_path, path)
    }
}

fn resolve_security_path(config_path: &Path, path: &Path) -> PathBuf {
    resolve_ip2region_path(config_path, path)
}

fn configured_path(path: Option<&Path>) -> bool {
    path.is_some_and(|path| !path.as_os_str().is_empty())
}

fn validate_http_url(field: &str, value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value)
        .with_context(|| format!("{field} is not a valid URL: {value}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("{field} must use http or https, got {}", url.scheme());
    }
    Ok(())
}

pub fn ensure_config_file(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "unable to create configuration directory {}",
                parent.display()
            )
        })?;
    }

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("unable to create configuration file {}", path.display())
            });
        }
    };

    file.write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
        .with_context(|| format!("unable to write configuration file {}", path.display()))?;
    file.flush()
        .with_context(|| format!("unable to flush configuration file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("unable to sync configuration file {}", path.display()))?;
    Ok(true)
}

pub fn ensure_vhost_files(config_path: &Path) -> Result<()> {
    let directory = config_directory(config_path).join("vhosts");
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "unable to create vhost configuration directory {}",
            directory.display()
        )
    })?;

    for (name, template) in DEFAULT_VHOST_TEMPLATES {
        let path = directory.join(format!("{name}.toml"));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "unable to create vhost configuration file {}",
                        path.display()
                    )
                });
            }
        };
        file.write_all(template.as_bytes()).with_context(|| {
            format!(
                "unable to write vhost configuration file {}",
                path.display()
            )
        })?;
        file.flush().with_context(|| {
            format!(
                "unable to flush vhost configuration file {}",
                path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!("unable to sync vhost configuration file {}", path.display())
        })?;
    }
    Ok(())
}

pub fn config_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ip2region::IpLocation, security::Security};
    use std::net::{IpAddr, Ipv4Addr};

    fn location(country: &str, isp: &str) -> IpLocation {
        IpLocation {
            ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8)),
            country_code: Some(country.to_owned()),
            country_name: None,
            province: None,
            city: None,
            isp: Some(isp.to_owned()),
        }
    }

    fn security() -> Security {
        Security::open(&SecurityConfig {
            enabled: false,
            vpn_isp_contains: vec!["cloud".to_owned()],
            ..SecurityConfig::default()
        })
        .expect("test security configuration should load")
    }

    #[test]
    fn route_matches_player_and_isp_as_and_conditions() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [lines.mobile]
            host = "mobile.example.com"
            port = 25565

            [[rules]]
            priority = 10
            line = "mobile"
            countries = ["JP"]
            isp_contains = ["mobile"]
            players = ["herobrine", "dinnerbone"]
            "#,
        )
        .expect("test routing configuration should parse");
        routing
            .validate()
            .expect("test routing configuration should validate");
        let security = security();

        let route = routing
            .select_route_with_context(
                &location("JP", "Example Mobile"),
                Some("Herobrine"),
                &security,
                "203.0.113.8",
            )
            .expect("matching route should exist");
        assert_eq!(route.0, "mobile");

        let fallback = routing
            .select_route_with_context(
                &location("JP", "Example Mobile"),
                Some("Alex"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(fallback.0, "global");
    }

    #[test]
    fn route_supports_negative_and_security_conditions() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [lines.special]
            host = "special.example.com"
            port = 25565

            [[rules]]
            priority = 20
            line = "special"
            not_countries = ["CN"]
            not_players = ["blocked"]
            vpn = true
            "#,
        )
        .expect("test routing configuration should parse");
        routing
            .validate()
            .expect("test routing configuration should validate");
        let security = security();

        let route = routing
            .select_route_with_context(
                &location("JP", "Cloud Transit"),
                Some("allowed"),
                &security,
                "203.0.113.8",
            )
            .expect("matching route should exist");
        assert_eq!(route.0, "special");

        let fallback = routing
            .select_route_with_context(
                &location("CN", "Cloud Transit"),
                Some("allowed"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(fallback.0, "global");
    }

    #[test]
    fn group_selects_hosts_in_round_robin_order() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [[group]]
            priority = 10
            group_name = "cmcc-cluster"
            mode = "round_robin"
            resolve_srv = true
            hosts = ["cmcc-01.example.com", "cmcc-02.example.com"]

            [[rules]]
            group = "cmcc-cluster"
            countries = ["CN"]
            "#,
        )
        .expect("test routing configuration should parse");
        routing
            .validate()
            .expect("test routing configuration should validate");
        let security = security();

        let first = routing
            .select_route_with_context(
                &location("CN", "China Mobile"),
                None,
                &security,
                "203.0.113.8",
            )
            .expect("group route should exist");
        let second = routing
            .select_route_with_context(
                &location("CN", "China Mobile"),
                None,
                &security,
                "203.0.113.8",
            )
            .expect("group route should exist");
        assert_eq!(first.0, "cmcc-cluster");
        assert_eq!(first.1.host, "cmcc-01.example.com");
        assert!(first.1.resolve_srv);
        assert_eq!(second.1.host, "cmcc-02.example.com");
    }

    #[test]
    fn routing_defaults_srv_to_disabled_and_uses_minecraft_port_fallback() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "play.example.com"
            resolve_srv = true
            "#,
        )
        .expect("SRV routing configuration should parse");
        routing
            .validate()
            .expect("SRV routing configuration should validate");

        assert!(routing.lines["global"].resolve_srv);
        assert_eq!(routing.lines["global"].port, 25565);
    }

    #[test]
    fn routing_defaults_srv_to_false() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "play.example.com"
            "#,
        )
        .expect("default SRV routing configuration should parse");
        assert!(!routing.lines["global"].resolve_srv);
    }

    #[test]
    fn routing_accepts_srv_compatibility_aliases_per_target() {
        for field in ["minecraft_srv", "srv"] {
            let source = format!(
                r#"
                default_line = "global"

                [lines.global]
                host = "play.example.com"
                {field} = true
                "#
            );
            let routing: RoutingConfig =
                toml::from_str(&source).expect("SRV compatibility field should parse");
            assert!(routing.lines["global"].resolve_srv);
        }
    }

    #[test]
    fn route_selects_target_by_requested_host() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [lines.alpha]
            host = "alpha-backend.example.com"
            port = 25565

            [lines.beta]
            host = "beta-backend.example.com"
            port = 25565

            [[rules]]
            priority = 100
            line = "alpha"
            hosts = ["alpha.example.com"]

            [[rules]]
            priority = 100
            line = "beta"
            hosts = ["beta.example.com", "beta.example.net"]
            "#,
        )
        .expect("host routing configuration should parse");
        routing
            .validate()
            .expect("host routing configuration should validate");
        let security = security();

        let alpha = routing
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("ALPHA.EXAMPLE.COM."),
                &security,
                "203.0.113.8",
            )
            .expect("host route should exist");
        assert_eq!(alpha.0, "alpha");

        let beta = routing
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("beta.example.net"),
                &security,
                "203.0.113.8",
            )
            .expect("host route should exist");
        assert_eq!(beta.0, "beta");

        let fallback = routing
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("unknown.example.com"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(fallback.0, "global");
    }

    #[test]
    fn route_supports_requested_host_negation() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [lines.special]
            host = "special-backend.example.com"
            port = 25565

            [[rules]]
            priority = 100
            line = "special"
            not_hosts = ["blocked.example.com"]
            "#,
        )
        .expect("host negation configuration should parse");
        routing
            .validate()
            .expect("host negation configuration should validate");
        let security = security();

        let allowed = routing
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("allowed.example.com"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(allowed.0, "special");

        let blocked = routing
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("blocked.example.com"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(blocked.0, "global");
    }

    #[test]
    fn default_config_template_is_valid() {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        AppConfig::load(&config_path).expect("default template should load and validate");
    }

    #[test]
    fn vhosts_select_routing_file_by_requested_host() {
        let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let config = AppConfig::load(&config_path).expect("vhost template should load");
        let security = security();

        let beta = config
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("BETA."),
                &security,
                "203.0.113.8",
            )
            .expect("beta vhost should be selected");
        assert_eq!(beta.0, "beta/default");
        assert_eq!(beta.1.host, "beta-backend.example.com");

        let fallback = config
            .select_route_with_host_context(
                &location("JP", "Example ISP"),
                None,
                Some("unknown.example.com"),
                &security,
                "203.0.113.8",
            )
            .expect("default vhost should be selected");
        assert_eq!(fallback.0, "alpha/default");
    }

    #[test]
    fn route_supports_bang_negation_in_matcher_values() {
        let routing: RoutingConfig = toml::from_str(
            r#"
            default_line = "global"

            [lines.global]
            host = "global.example.com"
            port = 25565

            [lines.special]
            host = "special.example.com"
            port = 25565

            [[rules]]
            priority = 20
            line = "special"
            countries = ["JP", "!CN"]
            isp_contains = ["mobile", "!cloud"]
            players = ["!blocked"]
            "#,
        )
        .expect("test routing configuration should parse");
        routing
            .validate()
            .expect("test routing configuration should validate");
        let security = security();

        let match_route = routing
            .select_route_with_context(
                &location("JP", "Example Mobile"),
                Some("allowed"),
                &security,
                "203.0.113.8",
            )
            .expect("bang matcher should match");
        assert_eq!(match_route.0, "special");

        let excluded_country = routing
            .select_route_with_context(
                &location("CN", "Example Mobile"),
                Some("allowed"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(excluded_country.0, "global");

        let excluded_isp = routing
            .select_route_with_context(
                &location("JP", "Cloud Transit"),
                Some("allowed"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(excluded_isp.0, "global");

        let excluded_player = routing
            .select_route_with_context(
                &location("JP", "Example Mobile"),
                Some("blocked"),
                &security,
                "203.0.113.8",
            )
            .expect("default route should exist");
        assert_eq!(excluded_player.0, "global");
    }
}

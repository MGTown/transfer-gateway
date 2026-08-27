use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    ip2region::IpLocation,
    language::{DEFAULT_LANGUAGE_DIRECTORY, DEFAULT_LOCALE},
    protocol,
};

pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.example.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub ip2region: Ip2RegionConfig,
    pub routing: RoutingConfig,
    #[serde(default)]
    pub language: LanguageConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("unable to read configuration file {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid TOML in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.server.validate()?;
        self.ip2region.validate()?;
        self.routing.validate()?;
        self.language.validate()?;
        Ok(())
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
    #[serde(default = "default_ip2region_download_base_url")]
    pub download_base_url: String,
}

impl Default for Ip2RegionConfig {
    fn default() -> Self {
        Self {
            v4_db: default_ip2region_v4_db(),
            v6_db: default_ip2region_v6_db(),
            auto_download: default_ip2region_auto_download(),
            download_base_url: default_ip2region_download_base_url(),
        }
    }
}

impl Ip2RegionConfig {
    fn validate(&self) -> Result<()> {
        if self.auto_download {
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
pub struct RoutingConfig {
    #[serde(default = "default_line")]
    pub default_line: String,
    #[serde(default)]
    pub lines: BTreeMap<String, LineTarget>,
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}

impl RoutingConfig {
    fn validate(&self) -> Result<()> {
        if self.lines.is_empty() {
            bail!("routing.lines must contain at least one target");
        }
        if !self.lines.contains_key(&self.default_line) {
            bail!(
                "routing.default_line '{}' is not present in routing.lines",
                self.default_line
            );
        }

        for (name, target) in &self.lines {
            target.validate(name)?;
        }
        for (index, rule) in self.rules.iter().enumerate() {
            if !self.lines.contains_key(&rule.line) {
                bail!(
                    "routing.rules[{index}].line '{}' is not present in routing.lines",
                    rule.line
                );
            }
        }
        Ok(())
    }

    pub fn select_route<'a>(&'a self, location: &IpLocation) -> Option<(&'a str, &'a LineTarget)> {
        let mut selected: Option<(i32, usize, &str)> = None;

        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.matches(location) {
                continue;
            }

            let should_replace = selected
                .as_ref()
                .map(|(priority, _, _)| rule.priority > *priority)
                .unwrap_or(true);
            if should_replace {
                selected = Some((rule.priority, index, rule.line.as_str()));
            }
        }

        let line_name = selected
            .map(|(_, _, line)| line)
            .unwrap_or(self.default_line.as_str());
        self.lines
            .get_key_value(line_name)
            .map(|(name, target)| (name.as_str(), target))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LineTarget {
    pub host: String,
    pub port: u16,
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RouteRule {
    #[serde(default)]
    pub priority: i32,
    pub line: String,
    #[serde(default)]
    pub countries: Vec<String>,
    #[serde(default, alias = "subdivisions")]
    pub provinces: Vec<String>,
    #[serde(default)]
    pub cities: Vec<String>,
    #[serde(default, alias = "operator_contains")]
    pub isp_contains: Vec<String>,
}

impl RouteRule {
    fn matches(&self, location: &IpLocation) -> bool {
        matches_text_list(&self.countries, location.country_code.as_deref())
            && matches_text_list(&self.provinces, location.province.as_deref())
            && matches_text_list(&self.cities, location.city.as_deref())
            && self.matches_operator(location)
    }

    fn matches_operator(&self, location: &IpLocation) -> bool {
        if self.isp_contains.is_empty() {
            return true;
        }

        self.isp_contains.iter().any(|needle| {
            let needle = needle.to_lowercase();
            location
                .isp
                .as_deref()
                .map(|isp| isp.to_lowercase().contains(needle.as_str()))
                .unwrap_or(false)
        })
    }
}

fn matches_text_list(values: &[String], actual: Option<&str>) -> bool {
    values.is_empty()
        || actual
            .map(|actual| {
                values
                    .iter()
                    .any(|expected| expected.eq_ignore_ascii_case(actual))
            })
            .unwrap_or(false)
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

fn default_ip2region_download_base_url() -> String {
    "https://raw.githubusercontent.com/lionsoul2014/ip2region/v3.17.0/data".to_owned()
}

fn default_line() -> String {
    "default".to_owned()
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

pub fn config_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
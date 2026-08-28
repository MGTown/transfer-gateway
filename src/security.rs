use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use tokio::{fs as tokio_fs, io::AsyncWriteExt};
use uuid::Uuid;

use crate::{config::SecurityConfig, ip2region::IpLocation};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_DOWNLOAD_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Vpn,
    Tor,
    Spam,
}

impl BlockReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vpn => "VPN/Proxy",
            Self::Tor => "Tor",
            Self::Spam => "Spam IP",
        }
    }

    pub fn disconnect_key(self) -> &'static str {
        match self {
            Self::Vpn => "disconnect.vpn_blocked",
            Self::Tor => "disconnect.tor_blocked",
            Self::Spam => "disconnect.spam_blocked",
        }
    }
}

pub struct Security {
    enabled: bool,
    block_vpn: bool,
    block_tor: bool,
    block_spam: bool,
    allowlist: IpSet,
    vpn: IpSet,
    tor: IpSet,
    spam: IpSet,
    vpn_isp_contains: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedList {
    pub kind: &'static str,
    pub path: PathBuf,
}

impl Security {
    pub fn open(config: &SecurityConfig) -> Result<Self> {
        let allowlist = IpSet::from_networks(
            config
                .allowlist
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    parse_network(value).with_context(|| {
                        format!("security.allowlist[{index}] is not a valid IP/CIDR: {value}")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        );
        let vpn = if config.enabled && config.block_vpn {
            load_network_files([
                (config.vpn_ipv4_list.as_deref(), "VPN IPv4"),
                (config.vpn_ipv6_list.as_deref(), "VPN IPv6"),
            ])?
        } else {
            IpSet::default()
        };
        let tor = if config.enabled && config.block_tor {
            load_ip_file(config.tor_exit_list.as_deref(), "Tor exit")?
        } else {
            IpSet::default()
        };
        let spam = if config.enabled && config.block_spam {
            load_ip_file(config.spam_list.as_deref(), "Spam IP")?
        } else {
            IpSet::default()
        };
        let vpn_isp_contains = config
            .vpn_isp_contains
            .iter()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .collect();

        Ok(Self {
            enabled: config.enabled,
            block_vpn: config.block_vpn,
            block_tor: config.block_tor,
            block_spam: config.block_spam,
            allowlist,
            vpn,
            tor,
            spam,
            vpn_isp_contains,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled && (self.block_vpn || self.block_tor || self.block_spam)
    }

    pub fn has_data(&self) -> bool {
        !self.vpn.is_empty()
            || !self.tor.is_empty()
            || !self.spam.is_empty()
            || !self.vpn_isp_contains.is_empty()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.vpn.len(), self.tor.len(), self.spam.len())
    }

    pub fn check(&self, location: &IpLocation) -> Option<BlockReason> {
        if !self.is_enabled() || self.allowlist.contains(location.ip) {
            return None;
        }
        if self.block_tor && self.tor.contains(location.ip) {
            return Some(BlockReason::Tor);
        }
        if self.block_vpn
            && (self.vpn.contains(location.ip) || self.matches_vpn_isp(location.isp.as_deref()))
        {
            return Some(BlockReason::Vpn);
        }
        if self.block_spam && self.spam.contains(location.ip) {
            return Some(BlockReason::Spam);
        }
        None
    }

    fn matches_vpn_isp(&self, isp: Option<&str>) -> bool {
        let Some(isp) = isp else {
            return false;
        };
        let isp = isp.to_lowercase();
        self.vpn_isp_contains
            .iter()
            .any(|needle| isp.contains(needle))
    }
}

pub async fn download_missing(config: &SecurityConfig) -> Result<Vec<DownloadedList>> {
    if !config.enabled || !config.auto_download {
        return Ok(Vec::new());
    }

    let mut pending = Vec::new();
    if config.block_tor
        && let Some(path) = configured_path(config.tor_exit_list.as_deref())
        && !path.exists()
    {
        pending.push(DownloadItem {
            kind: "Tor exit",
            format: ListFormat::Ip,
            path: path.to_owned(),
            url: config.tor_exit_list_url.clone(),
        });
    }
    if config.block_spam
        && let Some(path) = configured_path(config.spam_list.as_deref())
        && !path.exists()
    {
        pending.push(DownloadItem {
            kind: "Spam IP",
            format: ListFormat::Network,
            path: path.to_owned(),
            url: config.spam_list_url.clone(),
        });
    }
    if config.block_vpn {
        if let Some(path) = configured_path(config.vpn_ipv4_list.as_deref())
            && !path.exists()
        {
            pending.push(DownloadItem {
                kind: "VPN IPv4",
                format: ListFormat::Network,
                path: path.to_owned(),
                url: config.vpn_ipv4_list_url.clone(),
            });
        }
        if let Some(path) = configured_path(config.vpn_ipv6_list.as_deref())
            && !path.exists()
        {
            pending.push(DownloadItem {
                kind: "VPN IPv6",
                format: ListFormat::Network,
                path: path.to_owned(),
                url: config.vpn_ipv6_list_url.clone(),
            });
        }
    }

    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let client = Client::builder()
        .user_agent(format!(
            "{}/{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(15))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("unable to create HTTP client for security list download")?;

    let mut downloaded = Vec::new();
    let mut errors = Vec::new();
    for item in pending {
        match download_one(&client, &item).await {
            Ok(true) => downloaded.push(DownloadedList {
                kind: item.kind,
                path: item.path,
            }),
            Ok(false) => {}
            Err(error) => errors.push(format!("{}: {error}", item.kind)),
        }
    }

    if errors.is_empty() {
        Ok(downloaded)
    } else {
        Err(anyhow!(
            "one or more security list downloads failed: {}",
            errors.join("; ")
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ListFormat {
    Ip,
    Network,
}

struct DownloadItem {
    kind: &'static str,
    format: ListFormat,
    path: PathBuf,
    url: String,
}

async fn download_one(client: &Client, item: &DownloadItem) -> Result<bool> {
    if item.path.exists() {
        return Ok(false);
    }

    if let Some(parent) = item.path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio_fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "unable to create security list directory {}",
                parent.display()
            )
        })?;
    }

    let temporary_path = temporary_path(&item.path);
    if let Err(error) = download_to_temp(client, &item.url, &temporary_path).await {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }
    if let Err(error) = validate_list_file(&temporary_path, item.kind, item.format) {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }

    let result = publish_download(&temporary_path, &item.path).await;
    if result.is_err() {
        let _ = tokio_fs::remove_file(&temporary_path).await;
    }
    result
}

async fn download_to_temp(client: &Client, url: &str, path: &Path) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("unable to download security list from {url}"))?;
    if !response.status().is_success() {
        bail!(
            "security list download from {url} returned HTTP {}",
            response.status()
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_SIZE)
    {
        bail!(
            "security list download from {url} is larger than {} bytes",
            MAX_DOWNLOAD_SIZE
        );
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "unable to create temporary security list file {}",
                path.display()
            )
        })?;
    let mut file = tokio_fs::File::from_std(file);
    let mut downloaded = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("unable to read security list download response")?
    {
        downloaded = downloaded
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!("security list download size overflow"))?;
        if downloaded > MAX_DOWNLOAD_SIZE {
            bail!(
                "security list download from {url} is larger than {} bytes",
                MAX_DOWNLOAD_SIZE
            );
        }
        file.write_all(&chunk).await.with_context(|| {
            format!(
                "unable to write temporary security list file {}",
                path.display()
            )
        })?;
    }
    file.flush().await.with_context(|| {
        format!(
            "unable to flush temporary security list file {}",
            path.display()
        )
    })?;
    file.sync_all().await.with_context(|| {
        format!(
            "unable to sync temporary security list file {}",
            path.display()
        )
    })?;
    Ok(())
}

fn validate_list_file(path: &Path, kind: &str, format: ListFormat) -> Result<()> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("unable to read downloaded {kind} list {}", path.display()))?;
    let entries = match format {
        ListFormat::Ip => parse_ip_lines(&source, path, kind)?,
        ListFormat::Network => parse_network_lines(&source, path, kind)?,
    };
    if entries.is_empty() {
        bail!("downloaded {kind} list {} is empty", path.display());
    }
    Ok(())
}

async fn publish_download(temporary_path: &Path, destination: &Path) -> Result<bool> {
    let source = tokio_fs::File::open(temporary_path)
        .await
        .with_context(|| {
            format!(
                "unable to open temporary security list {}",
                temporary_path.display()
            )
        })?;
    let destination_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "unable to publish security list to {}",
                    destination.display()
                )
            });
        }
    };

    let mut destination_file = tokio_fs::File::from_std(destination_file);
    let mut source = source;
    let result = async {
        tokio::io::copy(&mut source, &mut destination_file).await?;
        destination_file.flush().await?;
        destination_file.sync_all().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if let Err(error) = result {
        let _ = tokio_fs::remove_file(destination).await;
        return Err(error).with_context(|| {
            format!(
                "unable to finish publishing security list to {}",
                destination.display()
            )
        });
    }
    Ok(true)
}

fn load_network_files<'a, I>(files: I) -> Result<IpSet>
where
    I: IntoIterator<Item = (Option<&'a Path>, &'static str)>,
{
    let mut networks = Vec::new();
    for (path, kind) in files {
        let Some(path) = configured_path(path) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("unable to read {kind} list {}", path.display()))?;
        networks.extend(parse_network_lines(&source, path, kind)?);
    }
    Ok(IpSet::from_networks(networks))
}

fn load_ip_file(path: Option<&Path>, kind: &str) -> Result<IpSet> {
    let Some(path) = configured_path(path) else {
        return Ok(IpSet::default());
    };
    if !path.exists() {
        return Ok(IpSet::default());
    }
    let source = fs::read_to_string(path)
        .with_context(|| format!("unable to read {kind} list {}", path.display()))?;
    Ok(IpSet::from_networks(parse_network_lines(
        &source, path, kind,
    )?))
}

fn parse_network_lines(source: &str, path: &Path, kind: &str) -> Result<Vec<IpNetwork>> {
    source
        .lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let token = list_token(line);
            (!token.is_empty()).then_some((line_index + 1, token))
        })
        .map(|(line_number, value)| {
            parse_network(value).with_context(|| {
                format!(
                    "invalid {kind} entry at {}:{line_number}: {value}",
                    path.display()
                )
            })
        })
        .collect()
}

fn parse_ip_lines(source: &str, path: &Path, kind: &str) -> Result<Vec<IpNetwork>> {
    source
        .lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let token = list_token(line);
            (!token.is_empty()).then_some((line_index + 1, token))
        })
        .map(|(line_number, value)| {
            let ip = value.parse::<IpAddr>().with_context(|| {
                format!(
                    "invalid {kind} entry at {}:{line_number}: {value}",
                    path.display()
                )
            })?;
            Ok(IpNetwork::from_ip(ip))
        })
        .collect()
}

fn list_token(line: &str) -> &str {
    line.split('#')
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
}

fn parse_network(value: &str) -> Result<IpNetwork> {
    let mut parts = value.split('/');
    let ip = parts
        .next()
        .ok_or_else(|| anyhow!("missing IP address"))?
        .parse::<IpAddr>()
        .with_context(|| format!("invalid IP address in {value}"))?;
    let prefix = match parts.next() {
        Some(prefix) => prefix
            .parse::<u8>()
            .with_context(|| format!("invalid prefix in {value}"))?,
        None => match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        },
    };
    if parts.next().is_some() {
        bail!("too many '/' separators in {value}");
    }

    match ip {
        IpAddr::V4(ip) => {
            if prefix > 32 {
                bail!("IPv4 prefix must be between 0 and 32 in {value}");
            }
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << u32::from(32 - prefix)
            };
            Ok(IpNetwork::V4 {
                network: u32::from(ip) & mask,
                prefix,
            })
        }
        IpAddr::V6(ip) => {
            if prefix > 128 {
                bail!("IPv6 prefix must be between 0 and 128 in {value}");
            }
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << u32::from(128 - prefix)
            };
            Ok(IpNetwork::V6 {
                network: u128::from_be_bytes(ip.octets()) & mask,
                prefix,
            })
        }
    }
}

fn configured_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("security-list");
    path.with_file_name(format!(".{name}.{}.download", Uuid::new_v4().simple()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IpNetwork {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

impl IpNetwork {
    fn from_ip(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => Self::V4 {
                network: u32::from(ip),
                prefix: 32,
            },
            IpAddr::V6(ip) => Self::V6 {
                network: u128::from_be_bytes(ip.octets()),
                prefix: 128,
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
struct IpSet {
    v4: IpTrie<32>,
    v6: IpTrie<128>,
}

impl IpSet {
    fn from_networks(networks: Vec<IpNetwork>) -> Self {
        let mut set = Self::default();
        for network in networks {
            set.insert(network);
        }
        set
    }

    fn insert(&mut self, network: IpNetwork) {
        match network {
            IpNetwork::V4 { network, prefix } => self.v4.insert(u128::from(network), prefix),
            IpNetwork::V6 { network, prefix } => self.v6.insert(network, prefix),
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => self.v4.contains(u128::from(u32::from(ip))),
            IpAddr::V6(ip) => self.v6.contains(u128::from_be_bytes(ip.octets())),
        }
    }

    fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }
}

#[derive(Debug, Clone, Default)]
struct TrieNode {
    children: [Option<usize>; 2],
    terminal: bool,
}

#[derive(Debug, Clone)]
struct IpTrie<const BITS: usize> {
    nodes: Vec<TrieNode>,
    entries: usize,
}

impl<const BITS: usize> Default for IpTrie<BITS> {
    fn default() -> Self {
        Self {
            nodes: vec![TrieNode::default()],
            entries: 0,
        }
    }
}

impl<const BITS: usize> IpTrie<BITS> {
    fn insert(&mut self, value: u128, prefix: u8) {
        let mut node_index = 0;
        for depth in 0..usize::from(prefix) {
            let bit = ((value >> (BITS - 1 - depth)) & 1) as usize;
            let next = self.nodes[node_index].children[bit];
            node_index = match next {
                Some(next) => next,
                None => {
                    let next = self.nodes.len();
                    self.nodes.push(TrieNode::default());
                    self.nodes[node_index].children[bit] = Some(next);
                    next
                }
            };
        }

        if !self.nodes[node_index].terminal {
            self.nodes[node_index].terminal = true;
            self.entries += 1;
        }
    }

    fn contains(&self, value: u128) -> bool {
        let mut node_index = 0;
        if self.nodes[node_index].terminal {
            return true;
        }

        for depth in 0..BITS {
            let bit = ((value >> (BITS - 1 - depth)) & 1) as usize;
            let Some(next) = self.nodes[node_index].children[bit] else {
                return false;
            };
            node_index = next;
            if self.nodes[node_index].terminal {
                return true;
            }
        }
        false
    }

    fn is_empty(&self) -> bool {
        self.entries == 0
    }

    fn len(&self) -> usize {
        self.entries
    }
}
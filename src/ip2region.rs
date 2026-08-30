use std::{
    fs::OpenOptions,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use ip2region::{CachePolicy, Searcher};
use reqwest::Client;
use tokio::{
    fs as tokio_fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use uuid::Uuid;

use crate::config::Ip2RegionConfig;

const IPV4_DATABASE_NAME: &str = "ip2region_v4.xdb";
const IPV6_DATABASE_NAME: &str = "ip2region_v6.xdb";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_DOWNLOAD_SIZE: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpLocation {
    pub ip: IpAddr,
    pub country_code: Option<String>,
    pub country_name: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
    pub isp: Option<String>,
}

impl IpLocation {
    pub fn empty(ip: IpAddr) -> Self {
        Self {
            ip,
            country_code: None,
            country_name: None,
            province: None,
            city: None,
            isp: None,
        }
    }
}

pub struct Ip2Region {
    v4: Option<Searcher>,
    v6: Option<Searcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedDatabase {
    pub version: &'static str,
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub updated: Vec<UpdatedDatabase>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatedDatabase {
    pub version: &'static str,
    pub path: PathBuf,
}

impl Ip2Region {
    pub fn open(config: &Ip2RegionConfig) -> Result<Self> {
        Ok(Self {
            v4: open_searcher(config.v4_db.as_deref(), "IPv4")?,
            v6: open_searcher(config.v6_db.as_deref(), "IPv6")?,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.v4.is_some() || self.v6.is_some()
    }

    pub fn lookup(&self, ip: IpAddr) -> Result<IpLocation> {
        let region = match ip {
            IpAddr::V4(address) => self
                .v4
                .as_ref()
                .map(|searcher| searcher.search(address))
                .transpose()
                .with_context(|| format!("ip2region IPv4 lookup failed for {ip}"))?,
            IpAddr::V6(address) => self
                .v6
                .as_ref()
                .map(|searcher| searcher.search(address))
                .transpose()
                .with_context(|| format!("ip2region IPv6 lookup failed for {ip}"))?,
        };

        let mut location = IpLocation::empty(ip);
        if let Some(region) = region.filter(|region| !region.is_empty()) {
            parse_region(&mut location, &region);
        }
        Ok(location)
    }
}

pub async fn download_missing(config: &Ip2RegionConfig) -> Result<Vec<DownloadedDatabase>> {
    if !config.auto_download {
        return Ok(Vec::new());
    }

    let mut pending = Vec::new();
    if let Some(path) = configured_path(config.v4_db.as_deref())
        && !path.exists()
    {
        pending.push((path, IPV4_DATABASE_NAME, "IPv4"));
    }
    if let Some(path) = configured_path(config.v6_db.as_deref())
        && !path.exists()
    {
        pending.push((path, IPV6_DATABASE_NAME, "IPv6"));
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
        .context("unable to create HTTP client for ip2region download")?;

    let mut downloaded = Vec::new();
    for (path, filename, version) in pending {
        if download_one(&client, &config.download_base_url, path, filename, version).await? {
            downloaded.push(DownloadedDatabase {
                version,
                path: path.to_owned(),
            });
        }
    }
    Ok(downloaded)
}

pub async fn update(config: &Ip2RegionConfig) -> Result<UpdateReport> {
    let mut report = UpdateReport::default();
    if !config.auto_update {
        return Ok(report);
    }

    let paths = [
        (config.v4_db.as_deref(), IPV4_DATABASE_NAME, "IPv4"),
        (config.v6_db.as_deref(), IPV6_DATABASE_NAME, "IPv6"),
    ];
    if !paths
        .iter()
        .any(|(path, _, _)| configured_path(*path).is_some())
    {
        return Ok(report);
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
        .context("unable to create HTTP client for ip2region update")?;

    for (path, filename, version) in paths {
        let Some(path) = configured_path(path) else {
            continue;
        };
        match update_one(&client, &config.download_base_url, path, filename, version).await {
            Ok(true) => report.updated.push(UpdatedDatabase {
                version,
                path: path.to_owned(),
            }),
            Ok(false) => {}
            Err(error) => report.errors.push(format!("{version}: {error}")),
        }
    }

    Ok(report)
}

fn configured_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

async fn download_one(
    client: &Client,
    base_url: &str,
    path: &Path,
    filename: &str,
    version: &'static str,
) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio_fs::create_dir_all(parent).await.with_context(|| {
            format!("unable to create ip2region directory {}", parent.display())
        })?;
    }

    let temporary_path = temporary_path(path);
    let url = download_url(base_url, filename);
    let result = download_to_temp(client, &url, &temporary_path).await;
    if let Err(error) = result {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }

    if let Err(error) = validate_downloaded_xdb(&temporary_path, version) {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }

    let result = publish_download(&temporary_path, path).await;
    if result.is_err() {
        let _ = tokio_fs::remove_file(&temporary_path).await;
    }
    result
}

async fn update_one(
    client: &Client,
    base_url: &str,
    path: &Path,
    filename: &str,
    version: &str,
) -> Result<bool> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio_fs::create_dir_all(parent).await.with_context(|| {
            format!("unable to create ip2region directory {}", parent.display())
        })?;
    }

    let temporary_path = temporary_path(path);
    if let Err(error) =
        download_to_temp(client, &download_url(base_url, filename), &temporary_path).await
    {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }

    if let Err(error) = validate_downloaded_xdb(&temporary_path, version) {
        let _ = tokio_fs::remove_file(&temporary_path).await;
        return Err(error);
    }

    if path.exists() && files_equal(&temporary_path, path).await? {
        tokio_fs::remove_file(&temporary_path)
            .await
            .with_context(|| {
                format!(
                    "unable to remove unchanged temporary ip2region database {}",
                    temporary_path.display()
                )
            })?;
        return Ok(false);
    }

    replace_download(&temporary_path, path).await?;
    Ok(true)
}

async fn download_to_temp(client: &Client, url: &str, path: &Path) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("unable to download ip2region database from {url}"))?;
    if !response.status().is_success() {
        bail!(
            "ip2region database download from {url} returned HTTP {}",
            response.status()
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_SIZE)
    {
        bail!(
            "ip2region database download from {url} is larger than {} bytes",
            MAX_DOWNLOAD_SIZE
        );
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "unable to create temporary download file {}",
                path.display()
            )
        })?;
    let mut file = tokio_fs::File::from_std(file);
    let mut downloaded = 0_u64;

    while let Some(chunk) = response
        .chunk()
        .await
        .context("unable to read ip2region download response")?
    {
        downloaded = downloaded
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!("ip2region download size overflow"))?;
        if downloaded > MAX_DOWNLOAD_SIZE {
            bail!(
                "ip2region database download from {url} is larger than {} bytes",
                MAX_DOWNLOAD_SIZE
            );
        }
        file.write_all(&chunk).await.with_context(|| {
            format!("unable to write temporary download file {}", path.display())
        })?;
    }

    file.flush()
        .await
        .with_context(|| format!("unable to flush temporary download file {}", path.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("unable to sync temporary download file {}", path.display()))?;
    Ok(())
}

fn validate_downloaded_xdb(path: &Path, version: &str) -> Result<()> {
    let searcher = Searcher::new(
        path.to_string_lossy().into_owned(),
        CachePolicy::VectorIndex,
    )
    .with_context(|| format!("downloaded ip2region {version} database is invalid"))?;
    let result = match version {
        "IPv4" => searcher.search(Ipv4Addr::UNSPECIFIED),
        "IPv6" => searcher.search(Ipv6Addr::UNSPECIFIED),
        _ => return Err(anyhow!("unsupported ip2region database version {version}")),
    };
    result.map(|_| ()).map_err(|error| {
        anyhow!("downloaded ip2region {version} database has the wrong IP version: {error}")
    })
}

async fn publish_download(temporary_path: &Path, destination: &Path) -> Result<bool> {
    let destination_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let _ = tokio_fs::remove_file(temporary_path).await;
            return Ok(false);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "unable to create ip2region database {}",
                    destination.display()
                )
            });
        }
    };

    let mut source = match tokio_fs::File::open(temporary_path).await {
        Ok(source) => source,
        Err(error) => {
            let _ = tokio_fs::remove_file(destination).await;
            return Err(error).with_context(|| {
                format!(
                    "unable to open temporary download file {}",
                    temporary_path.display()
                )
            });
        }
    };
    let mut destination_file = tokio_fs::File::from_std(destination_file);

    if let Err(error) = tokio::io::copy(&mut source, &mut destination_file).await {
        drop(source);
        drop(destination_file);
        let _ = tokio_fs::remove_file(destination).await;
        return Err(error).with_context(|| {
            format!(
                "unable to publish ip2region database {}",
                destination.display()
            )
        });
    }
    if let Err(error) = destination_file.sync_all().await {
        drop(source);
        drop(destination_file);
        let _ = tokio_fs::remove_file(destination).await;
        return Err(error).with_context(|| {
            format!(
                "unable to sync ip2region database {}",
                destination.display()
            )
        });
    }

    drop(source);
    drop(destination_file);
    tokio_fs::remove_file(temporary_path)
        .await
        .with_context(|| {
            format!(
                "unable to remove temporary download file {}",
                temporary_path.display()
            )
        })?;
    Ok(true)
}

async fn replace_download(temporary_path: &Path, destination: &Path) -> Result<()> {
    tokio_fs::rename(temporary_path, destination)
        .await
        .with_context(|| {
            format!(
                "unable to atomically replace ip2region database {}",
                destination.display()
            )
        })
}

async fn files_equal(first: &Path, second: &Path) -> Result<bool> {
    let first_metadata = tokio_fs::metadata(first).await?;
    let second_metadata = tokio_fs::metadata(second).await?;
    if first_metadata.len() != second_metadata.len() {
        return Ok(false);
    }

    let mut first = tokio_fs::File::open(first).await?;
    let mut second = tokio_fs::File::open(second).await?;
    let mut first_buffer = [0_u8; 64 * 1024];
    let mut second_buffer = [0_u8; 64 * 1024];

    loop {
        let first_read = first.read(&mut first_buffer).await?;
        let second_read = second.read(&mut second_buffer).await?;
        if first_read != second_read {
            return Ok(false);
        }
        if first_read == 0 {
            return Ok(true);
        }
        if first_buffer[..first_read] != second_buffer[..second_read] {
            return Ok(false);
        }
    }
}

fn download_url(base_url: &str, filename: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), filename)
}

fn temporary_path(path: &Path) -> PathBuf {
    let filename = path
        .file_name()
        .and_then(|filename| filename.to_str())
        .unwrap_or("ip2region.xdb");
    path.with_file_name(format!(".{filename}.{}.part", Uuid::new_v4()))
}

fn open_searcher(path: Option<&Path>, version: &str) -> Result<Option<Searcher>> {
    let Some(path) = configured_path(path) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    Searcher::new(
        path.to_string_lossy().into_owned(),
        CachePolicy::VectorIndex,
    )
    .with_context(|| {
        format!(
            "unable to open ip2region {version} database {}",
            path.display()
        )
    })
    .map(Some)
}

fn parse_region(location: &mut IpLocation, region: &str) {
    let fields: Vec<_> = region.split('|').collect();

    location.country_name = nonzero(fields.first().copied().unwrap_or_default());
    location.province = nonzero(fields.get(1).copied().unwrap_or_default());
    location.city = nonzero(fields.get(2).copied().unwrap_or_default());
    location.isp = nonzero(fields.get(3).copied().unwrap_or_default());
    location.country_code = fields
        .get(4)
        .copied()
        .filter(|value| is_country_code(value))
        .and_then(nonzero);
}

fn nonzero(value: &str) -> Option<String> {
    (!value.is_empty() && value != "0").then(|| value.to_owned())
}

fn is_country_code(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic())
}

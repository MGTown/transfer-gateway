use std::{
    env,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use mc_transfer_gateway::{
    config::{self, AppConfig, LoggingConfig},
    ip2region::{self, Ip2Region},
    language::{self, Language},
    server,
};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    if !config_path.exists() && config::ensure_config_file(&config_path)? {
        let language_directory =
            config::config_directory(&config_path).join(language::DEFAULT_LANGUAGE_DIRECTORY);
        language::ensure_file(&language_directory.join("zh-CN.toml"), "zh-CN")?;
        language::ensure_file(&language_directory.join("en-US.toml"), "en-US")?;

        let bootstrap = Language::builtin(language::DEFAULT_LOCALE)?;
        let config_display = config_path.display().to_string();
        let directory_display = language_directory.display().to_string();
        println!(
            "{}",
            bootstrap.render(
                "log.first_run",
                &[
                    ("config", config_display.as_str()),
                    ("directory", directory_display.as_str()),
                ],
            )
        );
        return Ok(());
    }

    let mut config = AppConfig::load(&config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    config.ip2region.resolve_paths(&config_path);
    init_logging(&config_path, &config.logging)?;
    let language_path = config.language.file_path(&config_path);
    let language_created = language::ensure_file(&language_path, &config.language.locale)?;
    let language = if language_created {
        let bootstrap = Language::builtin(&config.language.locale)?;
        let path_display = language_path.display().to_string();
        info!(
            "{}",
            bootstrap.render("log.language_created", &[("path", path_display.as_str())])
        );
        Arc::new(Language::load(&language_path, &config.language.locale)?)
    } else {
        Arc::new(Language::load(&language_path, &config.language.locale)?)
    };
    if config.ip2region.auto_download {
        match ip2region::download_missing(&config.ip2region).await {
            Ok(downloaded) => {
                for database in downloaded {
                    let path = database.path.display().to_string();
                    info!(
                        "{}",
                        language.render(
                            "log.ip2region_downloaded",
                            &[("version", database.version), ("path", path.as_str())],
                        )
                    );
                }
            }
            Err(error) => {
                let error_display = error.to_string();
                tracing::error!(
                    "{}",
                    language.render(
                        "log.ip2region_download_failed",
                        &[("error", error_display.as_str())],
                    )
                );
            }
        }
    }
    let ip2region = Arc::new(Ip2Region::open(&config.ip2region)?);

    if !ip2region.is_configured() {
        tracing::warn!("{}", language.render("log.no_ip2region", &[]));
    }

    let start_message = language.render(
        "log.starting",
        &[("locale", config.language.locale.as_str())],
    );
    info!(
        bind = %config.server.bind,
        protocols = ?config.server.supported_protocols,
        "{start_message}"
    );

    server::run(Arc::new(config), ip2region, language).await
}

fn init_logging(config_path: &Path, logging: &LoggingConfig) -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mc_transfer_gateway=info"));

    let Some(log_path) = logging.file_path(config_path) else {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .init();
        return Ok(());
    };

    if let Some(parent) = log_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("unable to create log directory {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("unable to open log file {}", log_path.display()))?;

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stdout.and(file))
        .init();
    Ok(())
}

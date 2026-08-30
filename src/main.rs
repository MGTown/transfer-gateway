use std::{env, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use mc_transfer_gateway::{
    config::{self, AppConfig},
    language::{self, Language},
    logging::LogController,
    runtime::{ConfigWatcher, RuntimeState},
    server,
};
use tokio::sync::watch;
use tracing::info;

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

    let config = AppConfig::load(&config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    let log_controller = LogController::initialize(&config_path, &config.logging)?;
    let runtime = RuntimeState::load(config, &config_path).await?;
    let language = Arc::clone(&runtime.language);

    let start_message = language.render(
        "log.starting",
        &[("locale", runtime.config.language.locale.as_str())],
    );
    info!(
        bind = %runtime.config.server.bind,
        protocols = ?runtime.config.server.supported_protocols,
        "{start_message}"
    );

    let (state_sender, state_receiver) = watch::channel(Arc::new(runtime));
    let watcher = ConfigWatcher::new(&config_path)?;
    info!("configuration hot reload and ip2region auto-update are enabled");
    let watcher_task = tokio::spawn(watcher.run(state_sender, log_controller));
    let result = server::run(state_receiver).await;
    watcher_task.abort();
    let _ = watcher_task.await;
    result
}

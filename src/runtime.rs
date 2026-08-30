use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::AppConfig,
    ip2region::{self, Ip2Region},
    language::{self, Language},
    logging::LogController,
    security::{self, Security},
};

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(250);
const DISABLED_UPDATE_RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct RuntimeState {
    pub config: Arc<AppConfig>,
    pub ip2region: Arc<Ip2Region>,
    pub security: Arc<Security>,
    pub language: Arc<Language>,
}

impl RuntimeState {
    pub async fn load(mut config: AppConfig, config_path: &Path) -> Result<Self> {
        config.resolve_paths(config_path);

        let language_path = config.language.file_path(config_path);
        let language_created = language::ensure_file(&language_path, &config.language.locale)?;
        let language = Arc::new(Language::load(&language_path, &config.language.locale)?);
        if language_created {
            let path_display = language_path.display().to_string();
            info!(
                "{}",
                language.render("log.language_created", &[("path", path_display.as_str())])
            );
        }

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
                    error!(
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
            warn!("{}", language.render("log.no_ip2region", &[]));
        }

        if config.security.auto_download {
            match security::download_missing(&config.security).await {
                Ok(downloaded) => {
                    for list in downloaded {
                        let path = list.path.display().to_string();
                        info!(
                            "{}",
                            language.render(
                                "log.security_list_downloaded",
                                &[("kind", list.kind), ("path", path.as_str())],
                            )
                        );
                    }
                }
                Err(error) => {
                    let error_display = error.to_string();
                    error!(
                        "{}",
                        language.render(
                            "log.security_list_download_failed",
                            &[("error", error_display.as_str())],
                        )
                    );
                }
            }
        }
        let security = Arc::new(Security::open(&config.security)?);
        if security.is_enabled() && !security.has_data() {
            warn!("{}", language.render("log.no_security_data", &[]));
        }
        let (vpn_entries, tor_entries, spam_entries) = security.counts();
        info!(
            vpn_entries,
            tor_entries,
            spam_entries,
            enabled = security.is_enabled(),
            "security blocklists loaded"
        );

        Ok(Self {
            config: Arc::new(config),
            ip2region,
            security,
            language,
        })
    }

    fn with_ip2region(&self, ip2region: Ip2Region) -> Self {
        Self {
            config: Arc::clone(&self.config),
            ip2region: Arc::new(ip2region),
            security: Arc::clone(&self.security),
            language: Arc::clone(&self.language),
        }
    }

    fn with_security(&self, security: Security) -> Self {
        Self {
            config: Arc::clone(&self.config),
            ip2region: Arc::clone(&self.ip2region),
            security: Arc::new(security),
            language: Arc::clone(&self.language),
        }
    }
}

enum WatchMessage {
    ConfigChanged,
    Error(String),
}

pub struct ConfigWatcher {
    config_path: PathBuf,
    events: mpsc::UnboundedReceiver<WatchMessage>,
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(config_path: &Path) -> Result<Self> {
        let config_file_name = config_path
            .file_name()
            .ok_or_else(|| anyhow!("configuration path has no file name"))?
            .to_os_string();
        let watch_directory = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let (event_sender, events) = mpsc::unbounded_channel();
        let event_file_name = config_file_name.clone();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| match result {
                Ok(event) if is_config_event(&event, &event_file_name) => {
                    let _ = event_sender.send(WatchMessage::ConfigChanged);
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = event_sender.send(WatchMessage::Error(error.to_string()));
                }
            },
            notify::Config::default(),
        )
        .context("unable to create configuration file watcher")?;
        watcher
            .watch(watch_directory, RecursiveMode::NonRecursive)
            .with_context(|| {
                format!(
                    "unable to watch configuration directory {}",
                    watch_directory.display()
                )
            })?;

        Ok(Self {
            config_path: config_path.to_owned(),
            events,
            _watcher: watcher,
        })
    }

    pub async fn run(
        mut self,
        state_sender: watch::Sender<Arc<RuntimeState>>,
        log_controller: LogController,
    ) {
        let mut ip2region_sleep =
            Box::pin(sleep(next_ip2region_update_delay(&state_sender.borrow())));
        let mut security_sleep =
            Box::pin(sleep(next_security_update_delay(&state_sender.borrow())));

        loop {
            tokio::select! {
                message = self.events.recv() => {
                    match message {
                        Some(WatchMessage::ConfigChanged) => {
                            sleep(CONFIG_DEBOUNCE).await;
                            self.drain_events();
                            reload_config(&self.config_path, &state_sender, &log_controller).await;
                            ip2region_sleep.as_mut().reset(Instant::now() + next_ip2region_update_delay(&state_sender.borrow()));
                            security_sleep.as_mut().reset(Instant::now() + next_security_update_delay(&state_sender.borrow()));
                        }
                        Some(WatchMessage::Error(error)) => {
                            warn!(?error, "configuration watcher reported an error");
                        }
                        None => return,
                    }
                }
                _ = &mut ip2region_sleep => {
                    update_ip2region(&state_sender).await;
                    ip2region_sleep.as_mut().reset(Instant::now() + next_ip2region_update_delay(&state_sender.borrow()));
                }
                _ = &mut security_sleep => {
                    update_security(&state_sender).await;
                    security_sleep.as_mut().reset(Instant::now() + next_security_update_delay(&state_sender.borrow()));
                }
            }
        }
    }

    fn drain_events(&mut self) {
        while let Ok(message) = self.events.try_recv() {
            if let WatchMessage::Error(error) = message {
                warn!(?error, "configuration watcher reported an error");
            }
        }
    }
}

fn is_config_event(event: &Event, file_name: &OsString) -> bool {
    if !matches!(
        event.kind,
        EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|path| {
        path.file_name()
            .is_some_and(|event_file_name| event_file_name == file_name)
    })
}

async fn reload_config(
    config_path: &Path,
    state_sender: &watch::Sender<Arc<RuntimeState>>,
    log_controller: &LogController,
) {
    let previous = state_sender.borrow().clone();
    let config = match AppConfig::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            log_reload_failure(&previous, error);
            return;
        }
    };
    let next = match RuntimeState::load(config, config_path).await {
        Ok(next) => next,
        Err(error) => {
            log_reload_failure(&previous, error);
            return;
        }
    };
    if let Err(error) = log_controller.apply(config_path, &next.config.logging) {
        log_reload_failure(&previous, error);
        return;
    }

    let message = next.language.render("log.config_reloaded", &[]);
    state_sender.send_replace(Arc::new(next));
    info!("{message}");
}

fn log_reload_failure(previous: &RuntimeState, error: impl std::fmt::Display) {
    let error_display = error.to_string();
    let message = previous.language.render(
        "log.config_reload_failed",
        &[("error", error_display.as_str())],
    );
    error!("{message}");
}

async fn update_ip2region(state_sender: &watch::Sender<Arc<RuntimeState>>) {
    let current = state_sender.borrow().clone();
    if !current.config.ip2region.auto_update {
        return;
    }

    let report = match ip2region::update(&current.config.ip2region).await {
        Ok(report) => report,
        Err(error) => {
            log_ip2region_update_failure(&current, error);
            return;
        }
    };
    for update_error in report.errors {
        log_ip2region_update_failure(&current, update_error);
    }
    if report.updated.is_empty() {
        debug!("ip2region databases are already up to date");
        return;
    }

    let updated = report.updated;
    let database = match Ip2Region::open(&current.config.ip2region) {
        Ok(database) => database,
        Err(error) => {
            log_ip2region_update_failure(&current, error);
            return;
        }
    };
    let next = Arc::new(current.with_ip2region(database));
    state_sender.send_replace(next);
    for database in updated {
        let path = database.path.display().to_string();
        info!(
            "{}",
            current.language.render(
                "log.ip2region_updated",
                &[("version", database.version), ("path", path.as_str())],
            )
        );
    }
}

async fn update_security(state_sender: &watch::Sender<Arc<RuntimeState>>) {
    let current = state_sender.borrow().clone();
    if !current.config.security.auto_update {
        return;
    }

    let report = match security::update(&current.config.security).await {
        Ok(report) => report,
        Err(error) => {
            log_security_update_failure(&current, error);
            return;
        }
    };
    for update_error in report.errors {
        log_security_update_failure(&current, update_error);
    }
    if report.updated.is_empty() {
        debug!("security lists are already up to date");
        return;
    }

    let updated = report.updated;
    let security = match Security::open(&current.config.security) {
        Ok(security) => security,
        Err(error) => {
            log_security_update_failure(&current, error);
            return;
        }
    };
    let next = Arc::new(current.with_security(security));
    state_sender.send_replace(next);
    for list in updated {
        let path = list.path.display().to_string();
        info!(
            "{}",
            current.language.render(
                "log.security_list_updated",
                &[("kind", list.kind), ("path", path.as_str())],
            )
        );
    }
}

fn log_security_update_failure(state: &RuntimeState, error: impl std::fmt::Display) {
    let error_display = error.to_string();
    error!(
        "{}",
        state.language.render(
            "log.security_list_update_failed",
            &[("error", error_display.as_str())],
        )
    );
}

fn log_ip2region_update_failure(state: &RuntimeState, error: impl std::fmt::Display) {
    let error_display = error.to_string();
    error!(
        "{}",
        state.language.render(
            "log.ip2region_update_failed",
            &[("error", error_display.as_str())],
        )
    );
}

fn next_ip2region_update_delay(state: &RuntimeState) -> Duration {
    if state.config.ip2region.auto_update {
        Duration::from_secs(state.config.ip2region.update_interval_secs)
    } else {
        DISABLED_UPDATE_RETRY
    }
}

fn next_security_update_delay(state: &RuntimeState) -> Duration {
    if state.config.security.auto_update {
        Duration::from_secs(state.config.security.update_interval_secs)
    } else {
        DISABLED_UPDATE_RETRY
    }
}

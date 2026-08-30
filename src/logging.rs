use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::config::LoggingConfig;

#[derive(Clone)]
pub struct LogController {
    state: Arc<Mutex<LogState>>,
}

struct LogState {
    file: Option<File>,
}

impl LogController {
    pub fn initialize(config_path: &Path, logging: &LoggingConfig) -> Result<Self> {
        let controller = Self {
            state: Arc::new(Mutex::new(LogState { file: None })),
        };
        controller.apply(config_path, logging)?;

        let writer_controller = controller.clone();
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("mc_transfer_gateway=info"));

        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(move || DynamicWriter {
                controller: writer_controller.clone(),
            })
            .init();

        Ok(controller)
    }

    pub fn apply(&self, config_path: &Path, logging: &LoggingConfig) -> Result<()> {
        let file = logging
            .file_path(config_path)
            .map(|path| open_log_file(&path))
            .transpose()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("log controller mutex is poisoned"))?;
        state.file = file;
        Ok(())
    }
}

struct DynamicWriter {
    controller: LogController,
}

impl Write for DynamicWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut state = self
            .controller
            .state
            .lock()
            .map_err(|_| io::Error::other("log controller mutex is poisoned"))?;
        io::stdout().write_all(buffer)?;
        if let Some(file) = state.file.as_mut() {
            file.write_all(buffer)?;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .controller
            .state
            .lock()
            .map_err(|_| io::Error::other("log controller mutex is poisoned"))?;
        io::stdout().flush()?;
        if let Some(file) = state.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

fn open_log_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("unable to create log directory {}", parent.display()))?;
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("unable to open log file {}", path.display()))
}

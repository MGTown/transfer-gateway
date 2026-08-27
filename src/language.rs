use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_LOCALE: &str = "zh-CN";
pub const DEFAULT_LANGUAGE_DIRECTORY: &str = "./lang";

const BUILTIN_ZH_CN: &str = include_str!("../lang/zh-CN.toml");
const BUILTIN_EN_US: &str = include_str!("../lang/en-US.toml");

#[derive(Debug, Clone)]
pub struct Language {
    pub locale: String,
    messages: BTreeMap<String, String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
struct LanguageDocument {
    #[serde(default)]
    meta: LanguageMeta,
    #[serde(default)]
    messages: BTreeMap<String, String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
struct LanguageMeta {
    #[serde(default)]
    locale: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: u32,
}

impl Language {
    pub fn load(path: &Path, locale: &str) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("unable to read language file {}", path.display()))?;
        Self::from_source(locale, &source)
            .with_context(|| format!("invalid language file {}", path.display()))
    }

    pub fn from_source(locale: &str, source: &str) -> Result<Self> {
        let selected: LanguageDocument = toml::from_str(source)?;
        let fallback: LanguageDocument = toml::from_str(BUILTIN_EN_US)
            .expect("built-in en-US language file must contain valid TOML");

        let mut messages = fallback.messages;
        messages.extend(selected.messages);

        Ok(Self {
            locale: locale.to_owned(),
            messages,
        })
    }

    pub fn builtin(locale: &str) -> Result<Self> {
        Self::from_source(locale, builtin_template(locale))
    }

    pub fn render(&self, key: &str, replacements: &[(&str, &str)]) -> String {
        let mut message = self
            .messages
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_owned());

        for (name, value) in replacements {
            message = message.replace(&format!("{{{name}}}"), value);
        }
        message
    }
}

pub fn builtin_template(locale: &str) -> &'static str {
    match locale {
        "zh-CN" => BUILTIN_ZH_CN,
        "en-US" => BUILTIN_EN_US,
        _ => BUILTIN_EN_US,
    }
}

pub fn ensure_file(path: &Path, locale: &str) -> Result<bool> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("unable to create language directory {}", parent.display()))?;
    }

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("unable to create language file {}", path.display()));
        }
    };

    file.write_all(builtin_template(locale).as_bytes())
        .with_context(|| format!("unable to write language file {}", path.display()))?;
    file.flush()
        .with_context(|| format!("unable to flush language file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("unable to sync language file {}", path.display()))?;
    Ok(true)
}
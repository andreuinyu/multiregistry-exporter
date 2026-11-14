use std::{env, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::Url;

use crate::image::{Image, parse_specs};

pub struct Config {
    pub images: Vec<Image>,
    pub interval: Duration,
    pub port: u16,
    pub tag: String,
    pub platform: String,
    pub version: String,
    pub log_level: String,
    pub dockerhub_api: Url,
    pub github_web: Url,
    pub github_api: Url,
    pub ghcr_registry: Url,
    pub github_token: Option<String>,
    pub dockerhub_user: Option<String>,
    pub dockerhub_token: Option<String>,
}

impl Config {
    pub fn from_env(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let args: Vec<String> = args.into_iter().collect();
        let specs = if args.is_empty() {
            env_string("IMAGES", "")?
        } else {
            args.join(" ")
        };
        let images = parse_specs(&specs).map_err(anyhow::Error::msg)?;
        if images.is_empty() {
            bail!(
                "no images configured. Set IMAGES=registry:owner/image"
            );
        }

        let interval = env_parse("INTERVAL", 1800_u64)?;
        let port = env_parse("PORT", 9101_u16)?;

        Ok(Self {
            images,
            interval: Duration::from_secs(interval),
            port,
            tag: env_string("TAG", "latest")?,
            platform: env_string("PLATFORM", "linux/amd64")?,
            version: env_string("APP_VERSION", "dev")?,
            log_level: env_string("LOG_LEVEL", "INFO")?,
            dockerhub_api: env_url("DOCKERHUB_API_URL", "https://hub.docker.com")?,
            github_web: env_url("GITHUB_WEB_URL", "https://github.com")?,
            github_api: env_url("GITHUB_API_URL", "https://api.github.com")?,
            ghcr_registry: env_url("GHCR_REGISTRY_URL", "https://ghcr.io")?,
            github_token: env_optional("GITHUB_TOKEN")?,
            dockerhub_user: env_optional("DOCKERHUB_USER")?,
            dockerhub_token: env_optional("DOCKERHUB_TOKEN")?,
        })
    }
}

fn env_string(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn env_optional(name: &str) -> Result<Option<String>> {
    env_string(name, "").map(|value| (!value.is_empty()).then_some(value))
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid {name} value {value:?}: {error}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn env_url(name: &str, default: &str) -> Result<Url> {
    let value = env_string(name, default)?;
    Url::parse(&value).with_context(|| format!("invalid {name} URL {value:?}"))
}


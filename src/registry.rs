use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::Regex;
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    config::Config,
    image::{Image, Registry},
};

pub type Metrics = HashMap<String, f64>;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

static DOWNLOADS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"Total downloads</span>\s*<h3[^>]*title="(\d+)""#)
        .expect("valid download-count expression")
});
static PUBLISHED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"Last published\s*</span>\s*<h3[^>]*title="([^"]+)""#)
        .expect("valid last-published expression")
});

pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new(version: &str) -> Result<Self> {
        let user_agent = format!("Mozilla/5.0 (compatible; multiregistry-exporter/{version})");
        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(20))
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self { client })
    }

    async fn get(&self, url: &Url, headers: &HeaderMap) -> Result<Response> {
        let started = Instant::now();
        let result = self
            .client
            .get(url.clone())
            .headers(headers.clone())
            .send()
            .await;

        debug!(
            url = %url,
            elapsed_seconds = started.elapsed().as_secs_f64(),
            "HTTP GET completed"
        );

        result.with_context(|| format!("HTTP GET {url} failed"))
    }

    async fn get_json<T>(&self, url: &Url, headers: &HeaderMap) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .get(url, headers)
            .await?
            .error_for_status()
            .with_context(|| format!("HTTP GET {url} returned an error status"))?;
        response
            .json()
            .await
            .with_context(|| format!("failed to decode JSON from {url}"))
    }
}

pub async fn fetch_image(http: &HttpClient, config: &Config, image: &Image) -> Result<Metrics> {
    match image.registry {
        Registry::DockerHub => fetch_dockerhub(http, config, image).await,
        Registry::Ghcr => fetch_ghcr(http, config, image).await,
    }
}

async fn fetch_dockerhub(http: &HttpClient, config: &Config, image: &Image) -> Result<Metrics> {
    let headers = dockerhub_headers(config)?;
    let mut repo_path = vec!["v2", "repositories", image.owner.as_str()];
    repo_path.extend(image.name.split('/'));
    repo_path.push("");
    let repository_url = append_path(&config.dockerhub_api, &repo_path)?;
    let (repository, size) = tokio::join!(
        http.get_json::<DockerHubRepository>(&repository_url, &headers),
        dockerhub_tag_size(http, config, image, &headers)
    );
    let repository = repository?;

    let mut metrics = Metrics::new();
    insert_optional(
        &mut metrics,
        "registry_image_pull_count",
        repository.pull_count,
    );
    insert_optional(&mut metrics, "registry_image_stars", repository.star_count);
    insert_timestamp(
        &mut metrics,
        "registry_image_last_updated_timestamp_seconds",
        repository.last_updated.as_deref(),
    );
    insert_timestamp(
        &mut metrics,
        "registry_image_created_timestamp_seconds",
        repository.date_registered.as_deref(),
    );

    match size {
        Ok(Some(size)) => {
            metrics.insert("registry_image_size_bytes".into(), size);
        }
        Ok(None) => {}
        Err(error) => warn!(%image, %error, "Docker Hub tag size fetch failed"),
    }

    Ok(metrics)
}

async fn dockerhub_tag_size(
    http: &HttpClient,
    config: &Config,
    image: &Image,
    headers: &HeaderMap,
) -> Result<Option<f64>> {
    let mut path = vec!["v2", "repositories", image.owner.as_str()];
    path.extend(image.name.split('/'));
    path.extend(["tags", config.tag.as_str()]);
    let url = append_path(&config.dockerhub_api, &path)?;
    let tag: DockerHubTag = http.get_json(&url, headers).await?;

    let (operating_system, architecture) = platform_parts(&config.platform);
    if let Some(images) = tag.images.filter(|images| !images.is_empty()) {
        for image in images {
            if image.os.as_deref() == Some(operating_system)
                && image.architecture.as_deref() == Some(architecture)
                && image.size.is_some_and(|size| size != 0.0)
            {
                return Ok(image.size);
            }
        }
        Ok(None)
    } else {
        Ok(tag.full_size)
    }
}

async fn fetch_ghcr(http: &HttpClient, config: &Config, image: &Image) -> Result<Metrics> {
    let mut html = None;
    let mut not_found = None;

    for kind in ["orgs", "users"] {
        let url = append_path(
            &config.github_web,
            &[
                kind,
                image.owner.as_str(),
                "packages",
                "container",
                "package",
                image.name.as_str(),
            ],
        )?;
        let response = http.get(&url, &HeaderMap::new()).await?;
        if response.status() == StatusCode::NOT_FOUND {
            not_found = Some(format!("HTTP GET {url} returned 404"));
            continue;
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("HTTP GET {url} returned an error status"))?;
        html = Some(
            response
                .text()
                .await
                .with_context(|| format!("failed to read response from {url}"))?,
        );
        break;
    }

    let html = html.ok_or_else(|| {
        anyhow::anyhow!(
            "{}",
            not_found.unwrap_or_else(|| "GitHub package page not found".to_owned())
        )
    })?;
    let downloads = DOWNLOADS_REGEX
        .captures(&html)
        .and_then(|captures| captures.get(1))
        .ok_or_else(|| {
            anyhow::anyhow!("download count not found on page (GitHub markup changed?)")
        })?
        .as_str()
        .parse::<f64>()
        .context("invalid download count on GitHub package page")?;

    let mut metrics = Metrics::new();
    metrics.insert("registry_image_pull_count".into(), downloads);
    if let Some(timestamp) = PUBLISHED_REGEX
        .captures(&html)
        .and_then(|captures| captures.get(1))
        .and_then(|value| iso_to_epoch(value.as_str()))
    {
        metrics.insert(
            "registry_image_last_updated_timestamp_seconds".into(),
            timestamp,
        );
    }

    let version_count = async {
        match config.github_token.as_deref() {
            Some(token) => ghcr_version_count(http, config, image, token)
                .await
                .map(Some),
            None => Ok(None),
        }
    };
    let (size, version_count) = tokio::join!(ghcr_image_size(http, config, image), version_count);

    match size {
        Ok(size) => {
            metrics.insert("registry_image_size_bytes".into(), size);
        }
        Err(error) => warn!(%image, %error, "GHCR image size fetch failed"),
    }

    match version_count {
        Ok(Some(count)) => {
            metrics.insert("registry_image_version_count".into(), count as f64);
        }
        Ok(None) => {}
        Err(error) => warn!(%image, %error, "GHCR version count fetch failed"),
    }

    Ok(metrics)
}

async fn ghcr_image_size(http: &HttpClient, config: &Config, image: &Image) -> Result<f64> {
    let mut token_url = append_path(&config.ghcr_registry, &["token"])?;
    token_url
        .query_pairs_mut()
        .append_pair("service", "ghcr.io")
        .append_pair(
            "scope",
            &format!("repository:{}/{}:pull", image.owner, image.name),
        );
    let token: TokenResponse = http.get_json(&token_url, &HeaderMap::new()).await?;
    let token = token
        .token
        .ok_or_else(|| anyhow::anyhow!("GHCR token response did not contain a token"))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid authorization token returned by GHCR")?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static(MANIFEST_ACCEPT));

    let mut repository_path = vec!["v2", image.owner.as_str()];
    repository_path.extend(image.name.split('/'));
    repository_path.push("manifests");
    repository_path.push(config.tag.as_str());
    let manifest_url = append_path(&config.ghcr_registry, &repository_path)?;
    let mut manifest: RegistryManifest = http.get_json(&manifest_url, &headers).await?;

    if !manifest.manifests.is_empty() {
        let (operating_system, architecture) = platform_parts(&config.platform);
        let descriptor = manifest
            .manifests
            .iter()
            .find(|entry| {
                entry.platform.as_ref().is_some_and(|platform| {
                    platform.os.as_deref() == Some(operating_system)
                        && platform.architecture.as_deref() == Some(architecture)
                })
            })
            .ok_or_else(|| anyhow::anyhow!("no {} manifest in {}", config.platform, config.tag))?;

        let mut image_path = vec!["v2", image.owner.as_str()];
        image_path.extend(image.name.split('/'));
        image_path.push("manifests");
        image_path.push(descriptor.digest.as_str());
        let image_manifest_url = append_path(&config.ghcr_registry, &image_path)?;
        manifest = http.get_json(&image_manifest_url, &headers).await?;
    }

    let config_size = manifest
        .config
        .and_then(|config| config.size)
        .unwrap_or(0.0);
    Ok(config_size
        + manifest
            .layers
            .into_iter()
            .filter_map(|layer| layer.size)
            .sum::<f64>())
}

async fn ghcr_version_count(
    http: &HttpClient,
    config: &Config,
    image: &Image,
    token: &str,
) -> Result<usize> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).context("invalid GITHUB_TOKEN header")?,
    );

    for kind in ["orgs", "users"] {
        let mut total = 0;
        let mut page = 1;
        loop {
            let url = append_path(
                &config.github_api,
                &[
                    kind,
                    image.owner.as_str(),
                    "packages",
                    "container",
                    image.name.as_str(),
                    "versions",
                ],
            )?;
            let mut url = url;
            url.query_pairs_mut()
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());

            let response = http.get(&url, &headers).await?;
            if response.status() == StatusCode::NOT_FOUND {
                break;
            }
            let response = response
                .error_for_status()
                .with_context(|| format!("HTTP GET {url} returned an error status"))?;
            let batch: Vec<serde_json::Value> = response
                .json()
                .await
                .with_context(|| format!("failed to decode package versions from {url}"))?;
            total += batch.len();
            if batch.len() < 100 {
                return Ok(total);
            }
            page += 1;
        }
    }

    bail!("package not found via GitHub API")
}

fn dockerhub_headers(config: &Config) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if let (Some(user), Some(token)) = (
        config.dockerhub_user.as_deref(),
        config.dockerhub_token.as_deref(),
    ) {
        let credentials = STANDARD.encode(format!("{user}:{token}"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {credentials}"))
                .context("invalid Docker Hub authorization header")?,
        );
    }
    Ok(headers)
}

fn append_path(base: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("base URL cannot accept path segments: {base}"))?
        .pop_if_empty()
        .extend(segments.iter().copied());
    Ok(url)
}

fn platform_parts(platform: &str) -> (&str, &str) {
    let mut parts = platform.split('/');
    (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    )
}

fn insert_optional(metrics: &mut Metrics, name: &str, value: Option<f64>) {
    if let Some(value) = value {
        metrics.insert(name.to_owned(), value);
    }
}

fn insert_timestamp(metrics: &mut Metrics, name: &str, value: Option<&str>) {
    if let Some(value) = value.and_then(iso_to_epoch) {
        metrics.insert(name.to_owned(), value);
    }
}

fn iso_to_epoch(value: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| {
            timestamp.timestamp() as f64 + f64::from(timestamp.timestamp_subsec_nanos()) / 1e9
        })
}

#[derive(Deserialize)]
struct DockerHubRepository {
    pull_count: Option<f64>,
    star_count: Option<f64>,
    last_updated: Option<String>,
    date_registered: Option<String>,
}

#[derive(Deserialize)]
struct DockerHubTag {
    images: Option<Vec<DockerHubImage>>,
    full_size: Option<f64>,
}

#[derive(Deserialize)]
struct DockerHubImage {
    os: Option<String>,
    architecture: Option<String>,
    size: Option<f64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Deserialize)]
struct RegistryManifest {
    #[serde(default)]
    manifests: Vec<ManifestDescriptor>,
    config: Option<ManifestSize>,
    #[serde(default)]
    layers: Vec<ManifestSize>,
}

#[derive(Deserialize)]
struct ManifestDescriptor {
    digest: String,
    platform: Option<ManifestPlatform>,
}

#[derive(Deserialize)]
struct ManifestPlatform {
    os: Option<String>,
    architecture: Option<String>,
}

#[derive(Deserialize)]
struct ManifestSize {
    size: Option<f64>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        Json, Router,
        body::Body,
        http::{HeaderMap, StatusCode, Uri},
        response::{IntoResponse, Response},
    };
    use reqwest::Url;
    use serde_json::json;
    use tokio::net::TcpListener;

    use crate::{
        config::Config,
        image::{Image, Registry},
    };

    use super::{
        HttpClient, append_path, fetch_dockerhub, fetch_ghcr, iso_to_epoch, platform_parts,
    };

    #[test]
    fn appends_and_encodes_url_path_segments() {
        let base = Url::parse("https://example.test/").unwrap();
        let url = append_path(&base, &["v2", "package name", "a/b"]).unwrap();
        assert_eq!(url.as_str(), "https://example.test/v2/package%20name/a%2Fb");
    }

    #[test]
    fn parses_rfc3339_timestamps_and_platforms() {
        assert_eq!(iso_to_epoch("1970-01-01T00:00:01Z"), Some(1.0));
        assert_eq!(iso_to_epoch("not a timestamp"), None);
        assert_eq!(platform_parts("linux/arm64"), ("linux", "arm64"));
        assert_eq!(platform_parts("linux"), ("linux", ""));
    }

    #[tokio::test]
    async fn dockerhub_fetch_uses_auth_and_requested_platform() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(mock_dockerhub))
                .await
                .unwrap();
        });
        let base = Url::parse(&format!("http://{address}")).unwrap();
        let mut config = test_config(&base);
        config.dockerhub_user = Some("user".into());
        config.dockerhub_token = Some("token".into());
        let image = Image {
            registry: Registry::DockerHub,
            owner: "library".into(),
            name: "nginx".into(),
        };
        let http = HttpClient::new("test").unwrap();

        let metrics = fetch_dockerhub(&http, &config, &image).await.unwrap();

        assert_eq!(metrics["registry_image_pull_count"], 1234.0);
        assert_eq!(metrics["registry_image_stars"], 56.0);
        assert_eq!(metrics["registry_image_size_bytes"], 4321.0);
        assert!(metrics.contains_key("registry_image_last_updated_timestamp_seconds"));
        assert!(metrics.contains_key("registry_image_created_timestamp_seconds"));
        server.abort();
    }

    #[tokio::test]
    async fn ghcr_fetches_downloads_and_manifest_size() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(mock_ghcr))
                .await
                .unwrap();
        });
        let base = Url::parse(&format!("http://{address}")).unwrap();
        let mut config = test_config(&base);
        config.github_token = Some("api-token".into());
        let image = Image {
            registry: Registry::Ghcr,
            owner: "owner".into(),
            name: "image".into(),
        };
        let http = HttpClient::new("test").unwrap();

        let metrics = fetch_ghcr(&http, &config, &image).await.unwrap();

        assert_eq!(metrics["registry_image_pull_count"], 987.0);
        assert_eq!(metrics["registry_image_size_bytes"], 600.0);
        assert_eq!(metrics["registry_image_version_count"], 2.0);
        assert!(metrics.contains_key("registry_image_last_updated_timestamp_seconds"));
        server.abort();
    }

    fn test_config(base: &Url) -> Config {
        Config {
            images: Vec::new(),
            interval: Duration::from_secs(1800),
            port: 9101,
            tag: "latest".into(),
            platform: "linux/amd64".into(),
            version: "test".into(),
            log_level: "INFO".into(),
            dockerhub_api: base.clone(),
            github_web: base.clone(),
            github_api: base.clone(),
            ghcr_registry: base.clone(),
            github_token: None,
            dockerhub_user: None,
            dockerhub_token: None,
        }
    }

    async fn mock_dockerhub(uri: Uri, headers: HeaderMap) -> Response<Body> {
        if headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            != Some("Mozilla/5.0 (compatible; multiregistry-exporter/test)")
        {
            return (StatusCode::BAD_REQUEST, "bad user agent").into_response();
        }
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some("Basic dXNlcjp0b2tlbg==")
        {
            return (StatusCode::UNAUTHORIZED, "missing auth").into_response();
        }

        match uri.path() {
            "/v2/repositories/library/nginx/" => Json(json!({
                "pull_count": 1234,
                "star_count": 56,
                "last_updated": "2024-01-02T03:04:05Z",
                "date_registered": "2020-01-02T03:04:05Z"
            }))
            .into_response(),
            "/v2/repositories/library/nginx/tags/latest" => Json(json!({
                "full_size": 9999,
                "images": [
                    {"os": "linux", "architecture": "arm64", "size": 999},
                    {"os": "linux", "architecture": "amd64", "size": 4321}
                ]
            }))
            .into_response(),
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    async fn mock_ghcr(uri: Uri, headers: HeaderMap) -> Response<Body> {
        if headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            != Some("Mozilla/5.0 (compatible; multiregistry-exporter/test)")
        {
            return (StatusCode::BAD_REQUEST, "bad user agent").into_response();
        }

        match uri.path() {
            "/orgs/owner/packages/container/package/image" => {
                (
                    [("content-type", "text/html")],
                    r#"Total downloads</span><h3 title="987">Last published </span><h3 title="2024-01-02T03:04:05Z">"#,
                )
                    .into_response()
            }
            "/token" => Json(json!({"token": "test-token"})).into_response(),
            "/v2/owner/image/manifests/latest" => Json(json!({
                "config": {"size": 100},
                "layers": [{"size": 200}, {"size": 300}]
            }))
            .into_response(),
            "/orgs/owner/packages/container/image/versions"
                if headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some("Bearer api-token") =>
            {
                Json(json!([{"id": 1}, {"id": 2}])).into_response()
            }
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }
}

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use tokio::{sync::RwLock, task::JoinSet, time::sleep};
use tracing::{info, warn};

use crate::{
    config::Config,
    image::Image,
    registry::{HttpClient, Metrics, fetch_image},
};

const MAX_CONCURRENT_IMAGES: usize = 4;

#[derive(Clone, Debug)]
pub struct ImagePoll {
    pub metrics: Metrics,
    pub up: bool,
    pub timestamp: f64,
    pub duration: f64,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct CollectorState {
    pub data: RwLock<HashMap<Image, ImagePoll>>,
}

pub async fn poll_loop(http: Arc<HttpClient>, config: Arc<Config>, state: Arc<CollectorState>) {
    loop {
        poll_once(Arc::clone(&http), Arc::clone(&config), Arc::clone(&state)).await;
        sleep(config.interval).await;
        info!(seconds = config.interval.as_secs(), "next poll scheduled");
    }
}

async fn poll_once(http: Arc<HttpClient>, config: Arc<Config>, state: Arc<CollectorState>) {
    let mut images = config.images.iter().cloned();
    let mut tasks = JoinSet::new();

    loop {
        while tasks.len() < MAX_CONCURRENT_IMAGES {
            let Some(image) = images.next() else {
                break;
            };
            let http = Arc::clone(&http);
            let config = Arc::clone(&config);
            tasks.spawn(async move {
                let started = Instant::now();
                let result = fetch_image(&http, &config, &image).await;
                let duration = started.elapsed().as_secs_f64();
                (image, result, duration)
            });
        }

        let Some(completion) = tasks.join_next().await else {
            break;
        };
        match completion {
            Ok((image, result, duration)) => {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|value| value.as_secs_f64())
                    .unwrap_or_default();
                let mut data = state.data.write().await;
                let old_metrics = data
                    .get(&image)
                    .map(|previous| previous.metrics.clone())
                    .unwrap_or_default();
                let (metrics, up, error) = handle_poll_result(result, old_metrics, &image);
                data.insert(
                    image,
                    ImagePoll {
                        metrics,
                        up,
                        timestamp,
                        duration,
                        error,
                    },
                );
                info!(elapsed_seconds = duration, "image poll completed");
            }
            Err(error) => warn!(%error, "image polling task failed"),
        }
    }
}

fn handle_poll_result(
    result: anyhow::Result<Metrics>,
    previous_metrics: Metrics,
    image: &Image,
) -> (Metrics, bool, Option<String>) {
    match result {
        Ok(metrics) if metrics.is_empty() => (previous_metrics, true, None),
        Ok(metrics) => (metrics, true, None),
        Err(error) => {
            warn!(%image, %error, "image poll failed");
            (previous_metrics, false, Some(format!("{error:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::handle_poll_result;
    use crate::image::{Image, Registry};
    use crate::registry::Metrics;

    #[test]
    fn failed_poll_preserves_last_good_metrics() {
        let image = Image {
            registry: Registry::DockerHub,
            owner: "library".into(),
            name: "nginx".into(),
        };
        let previous = HashMap::from([("registry_image_pull_count".into(), 12.0)]);
        let (metrics, up, error) = handle_poll_result(
            Err(anyhow::anyhow!("rate limited")),
            previous.clone(),
            &image,
        );

        assert_eq!(metrics, previous);
        assert!(!up);
        assert_eq!(error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn successful_poll_replaces_metrics_and_clears_error() {
        let image = Image {
            registry: Registry::DockerHub,
            owner: "library".into(),
            name: "nginx".into(),
        };
        let replacement: Metrics = HashMap::from([("registry_image_pull_count".into(), 42.0)]);
        let (metrics, up, error) =
            handle_poll_result(Ok(replacement.clone()), HashMap::new(), &image);

        assert_eq!(metrics, replacement);
        assert!(up);
        assert_eq!(error, None);
    }

    #[test]
    fn successful_poll_with_no_metrics_preserves_previous_values() {
        let image = Image {
            registry: Registry::DockerHub,
            owner: "library".into(),
            name: "nginx".into(),
        };
        let previous = HashMap::from([("registry_image_pull_count".into(), 12.0)]);
        let (metrics, up, error) = handle_poll_result(Ok(HashMap::new()), previous.clone(), &image);

        assert_eq!(metrics, previous);
        assert!(up);
        assert_eq!(error, None);
    }
}

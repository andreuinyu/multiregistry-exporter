use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use chrono::{DateTime, Utc};

use crate::{
    config::Config,
    image::Image,
    state::{CollectorState, ImagePoll},
};

const METRIC_HELP: &[(&str, &str)] = &[
    (
        "registry_image_pull_count",
        "Total pulls/downloads of the image.",
    ),
    (
        "registry_image_size_bytes",
        "Compressed size of the TAG image (default latest) for PLATFORM.",
    ),
    ("registry_image_stars", "Docker Hub star count."),
    (
        "registry_image_version_count",
        "Number of package versions (ghcr, needs GITHUB_TOKEN).",
    ),
    (
        "registry_image_last_updated_timestamp_seconds",
        "Unix time the image was last pushed/published.",
    ),
    (
        "registry_image_created_timestamp_seconds",
        "Unix time the repository was created.",
    ),
    (
        "registry_image_up",
        "1 if the last poll of this image succeeded.",
    ),
    (
        "registry_image_last_poll_timestamp_seconds",
        "Unix time of the last poll attempt.",
    ),
    (
        "registry_image_poll_duration_seconds",
        "Duration of the last poll for this image.",
    ),
];

#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub state: Arc<CollectorState>,
}

pub fn router(context: AppContext) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics))
        .route("/healthz", get(health))
        .fallback(not_found)
        .with_state(context)
}

async fn metrics(State(context): State<AppContext>) -> impl IntoResponse {
    let data = context.state.data.read().await.clone();
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        render_metrics(&context.config.images, &data, &context.config.version),
    )
}

async fn index(State(context): State<AppContext>) -> impl IntoResponse {
    let data = context.state.data.read().await.clone();
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        render_index(&context.config.images, &data, &context.config.version),
    )
}

async fn health() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))],
        "ok\n",
    )
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))],
        "not found\n",
    )
}

fn render_metrics(images: &[Image], data: &HashMap<Image, ImagePoll>, version: &str) -> String {
    let mut output = format!(
        "# HELP multiregistry_exporter_info Exporter build info.\n\
         # TYPE multiregistry_exporter_info gauge\n\
         multiregistry_exporter_info{{version=\"{}\"}} 1\n",
        escape_prometheus_label(version)
    );

    for (name, help) in METRIC_HELP {
        let mut samples = Vec::new();
        for image in images {
            let Some(poll) = data.get(image) else {
                continue;
            };
            let labels = format!("image=\"{}\"", escape_prometheus_label(&image.to_string()));
            let value = match *name {
                "registry_image_up" => Some(if poll.up { 1.0 } else { 0.0 }),
                "registry_image_last_poll_timestamp_seconds" => Some(poll.timestamp),
                "registry_image_poll_duration_seconds" => Some(poll.duration),
                _ => poll.metrics.get(*name).copied(),
            };
            if let Some(value) = value {
                samples.push(format!(
                    "{name}{{{labels}}} {}",
                    format_metric_value(name, value)
                ));
            }
        }

        if !samples.is_empty() {
            output.push_str(&format!(
                "\n# HELP {name} {help}\n# TYPE {name} gauge\n{}\n",
                samples.join("\n")
            ));
        }
    }

    output
}

fn render_index(images: &[Image], data: &HashMap<Image, ImagePoll>, version: &str) -> String {
    let mut rows = Vec::new();
    for image in images {
        let Some(poll) = data.get(image) else {
            rows.push(format!(
                "<tr><td>{}</td><td colspan=4>waiting for first poll…</td></tr>",
                escape_html(&image.to_string())
            ));
            continue;
        };

        let pulls = poll
            .metrics
            .get("registry_image_pull_count")
            .map(|value| format_thousands(*value))
            .unwrap_or_else(|| "–".to_owned());
        let updated = poll
            .metrics
            .get("registry_image_last_updated_timestamp_seconds")
            .and_then(|value| format_utc_timestamp(*value))
            .unwrap_or_else(|| "–".to_owned());
        let error = poll.error.as_deref().map(escape_html).unwrap_or_default();

        rows.push(format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&image.to_string()),
            if poll.up { "&#9989;" } else { "&#10060;" },
            pulls,
            updated,
            error
        ));
    }

    format!(
        "<!doctype html><meta charset=utf-8><title>multiregistry-exporter</title>\
         <style>body{{font:14px system-ui;margin:2em}}td,th{{padding:4px 12px;text-align:left}}\
         th{{border-bottom:1px solid #888}}</style>\
         <h2>multiregistry-exporter {}</h2>\
         <p><a href=/metrics>/metrics</a> · <a href=/healthz>/healthz</a></p>\
         <table><tr><th>image<th>ok<th>pulls<th>last updated<th>error</tr>{}</table>",
        escape_html(version),
        rows.join("")
    )
}

fn escape_prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn format_metric_value(name: &str, value: f64) -> String {
    match name {
        "registry_image_last_poll_timestamp_seconds" => format!("{value:.0}"),
        "registry_image_poll_duration_seconds" => format!("{value:.3}"),
        _ if value.fract() == 0.0 => format!("{value:.0}"),
        _ => value.to_string(),
    }
}

fn format_thousands(value: f64) -> String {
    let number = format!("{value:.0}");
    let negative = number.starts_with('-');
    let digits = number.trim_start_matches('-');
    let mut output = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    if negative {
        format!("-{output}")
    } else {
        output
    }
}

fn format_utc_timestamp(timestamp: f64) -> Option<String> {
    if timestamp <= 0.0 {
        return None;
    }
    DateTime::<Utc>::from_timestamp_millis((timestamp * 1000.0).round() as i64)
        .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ImagePoll, escape_html, render_index, render_metrics};
    use crate::image::{Image, Registry};

    #[test]
    fn renders_metrics_and_escapes_image_labels() {
        let image = Image {
            registry: Registry::DockerHub,
            owner: "team\"<x>".into(),
            name: "image".into(),
        };
        let data = HashMap::from([(
            image.clone(),
            ImagePoll {
                metrics: HashMap::from([("registry_image_pull_count".into(), 1234.0)]),
                up: true,
                timestamp: 1_700_000_000.0,
                duration: 0.25,
                error: None,
            },
        )]);

        let version = env!("CARGO_PKG_VERSION");
        let output = render_metrics(std::slice::from_ref(&image), &data, version);
        assert!(
            output.contains(&format!("multiregistry_exporter_info{{version=\"{version}\"}} 1"))
        );
        assert!(
            output
                .contains("registry_image_pull_count{image=\"dockerhub:team\\\"<x>/image\"} 1234")
        );
        assert!(output.contains("registry_image_up{image="));
    }

    #[test]
    fn renders_waiting_row_and_escapes_html() {
        let image = Image {
            registry: Registry::Ghcr,
            owner: "owner".into(),
            name: "<image>".into(),
        };
        let output = render_index(std::slice::from_ref(&image), &HashMap::new(), "dev");
        assert!(output.contains("&lt;image&gt;"));
        assert!(output.contains("waiting for first poll"));
        assert_eq!(escape_html("a&b"), "a&amp;b");
    }
}

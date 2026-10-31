# multiregistry-exporter

Prometheus exporter for container image stats from GHCR and Docker Hub, heavily inspired by [d0ugal's ghcr-exporter](https://github.com/d0ugal/ghcr-exporter) and [promhippie's dockerhub-exporter](https://github.com/promhippie/dockerhub_exporter).

The user list images as `registry:owner/image` and it serves pull counts, size and last-updated
time on `/metrics`. Python standard library only.

## Configuration

Images are set with the `IMAGES` env var (or as CLI arguments), separated by commas, spaces or newlines:

```
IMAGES="ghcr:linuxserver/sonarr,dockerhub:jellyfin/jellyfin"
```

Registries: `ghcr` (alias `ghcr.io`) and `dockerhub` (aliases `docker`, `hub`, `docker.io`).
`dockerhub:nginx` is shorthand for `dockerhub:library/nginx`.

| Variable | Default | Description |
|---|---|---|
| `IMAGES` | – | Images to track (required) |
| `INTERVAL` | `1800` | Seconds between registry polls (def. 30 min)|
| `PORT` | `9101` | Listen port |
| `TAG` | `latest` | Tag used for `registry_image_size_bytes` |
| `PLATFORM` | `linux/amd64` | Platform used for the size of multi-arch images |
| `GITHUB_TOKEN` | – | Optional. Enables `registry_image_version_count` for GHCR |
| `DOCKERHUB_USER`, `DOCKERHUB_TOKEN` | – | Optional. Authenticated Docker Hub requests (higher rate limits) |
| `LOG_LEVEL` | `INFO` | Log verbosity |

## Endpoints

- `/metrics` Prometheus metrics
- `/` HTML status table
- `/healthz` liveness check

## Docker Compose

### Without a published port

If Prometheus is on the same Docker network, it can reach the container by name and nothing
needs to be published. This example joins an existing network called `monitoring`
(the one Prometheus is attached to):

```yaml
services:
  multiregistry-exporter:
    container_name: multiregistry-exporter
    image: andreuinyu/multiregistry-exporter # or ghcr.io/andreuinyu/multiregistry-exporter:latest
    restart: unless-stopped
    environment:
      IMAGES: |
        ghcr:linuxserver/radarr
        dockerhub:jellyfin/jellyfin
        dockerhub:nginx
        <registry>:<owner>/<image>
```

If Prometheus is defined in the same compose file with no network specified; both services
share the default network.

Scrape target: `multiregistry-exporter:9101`

### With a published port

Use this when Prometheus runs outside Docker or on another host.

```yaml
services:
  multiregistry-exporter:
    container_name: multiregistry-exporter
    image: andreuinyu/multiregistry-exporter # or ghcr.io/andreuinyu/multiregistry-exporter:latest
    restart: unless-stopped
    ports:
      - "<your port of choice>:9101"
    environment:
      IMAGES: |
        ghcr:linuxserver/radarr
        dockerhub:jellyfin/jellyfin
        dockerhub:nginx
        <registry>:<owner>/<image>
```

Scrape target: `<docker-host>:<your port of choice>`

## Prometheus

```yaml
scrape_configs:
  - job_name: multiregistry-exporter
    scrape_interval: 5m
    static_configs:
      - targets: ["multiregistry-exporter:9101"] # or, "<docker-host>:<your port of choice>"
```

Values only change once per `INTERVAL`, so a short scrape interval gains nothing.

## Metrics

Every metric has one label, `image="registry:owner/name"`, matching the input format.
Aliases and shorthand are normalised (`dockerhub:nginx` becomes `dockerhub:library/nginx`).

| Metric | Registry | Description |
|---|---|---|
| `registry_image_pull_count` | both | Total pulls / downloads |
| `registry_image_size_bytes` | both | Compressed size of `TAG` for `PLATFORM` |
| `registry_image_last_updated_timestamp_seconds` | both | Last push / publish time |
| `registry_image_stars` | Docker Hub | Star count |
| `registry_image_created_timestamp_seconds` | Docker Hub | Repository creation time |
| `registry_image_version_count` | GHCR | Number of package versions (needs `GITHUB_TOKEN`) |
| `registry_image_up` | both | 1 if the last poll succeeded, else 0 |
| `registry_image_last_poll_timestamp_seconds` | both | Time of the last poll |
| `registry_image_poll_duration_seconds` | both | Duration of the last poll |
| `multiregistry_exporter_info{version}` | – | Exporter version |

## Behavior

- Results are cached and refreshed every `INTERVAL`. Scraping does not trigger registry requests.
- If a poll fails, the last known values are kept and `registry_image_up` is set to 0.
  The error is shown on `/` and in the logs.
- Metrics a registry does not provide are omitted rather than reported as 0.

## Data sources

- **Docker Hub:** the public API at `hub.docker.com/v2/repositories/<owner>/<image>/`
  (and `.../tags/<TAG>` for size).
- **GHCR, pulls and last published:** GitHub has no API for download counts, so these are read from the
  public package page (`github.com/orgs|users/<owner>/packages/container/package/<image>`).
  If GitHub changes that page, `registry_image_up` drops to 0.
- **GHCR, size:** computed from the image manifest (config + layers) through the ghcr.io registry API
  using an anonymous pull token.

## Build from source

```
docker build --build-arg VERSION=dev -t multiregistry-exporter .
```

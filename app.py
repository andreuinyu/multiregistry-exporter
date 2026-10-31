#!/usr/bin/env python3
"""
multiregistry-exporter: Prometheus exporter for container registry stats

Give it images as  <registry>:<owner>/<image>  e.g.
    ghcr:linuxserver/sonarr
    dockerhub:jellyfin/jellyfin
    dockerhub:nginx              (official image -> library/nginx)

and scrape http://multiregistry-exporter:9101/metrics

Config (env vars, or pass specs as CLI args):
    IMAGES            comma / space / newline separated image specs
    INTERVAL          seconds between registry polls         (default 1800)
    PORT              listen port                            (default 9101)
    TAG               tag whose size is reported            (default latest)
    PLATFORM          platform picked for multi-arch images (default linux/amd64)
    GITHUB_TOKEN      optional; adds ghcr version counts via the GitHub API
    DOCKERHUB_USER / DOCKERHUB_TOKEN   optional; Docker Hub auth for higher rate limits
    LOG_LEVEL         default INFO
"""
import base64
import json
import logging
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = os.environ.get("APP_VERSION", "dev")  # injected by the Dockerfile (ARG VERSION)
UA = "Mozilla/5.0 (compatible; multiregistry-exporter/%s)" % VERSION
log = logging.getLogger("multiregistry-exporter")

REGISTRY_ALIASES = {
    "ghcr": "ghcr", "ghcr.io": "ghcr", "github": "ghcr",
    "dockerhub": "dockerhub", "docker": "dockerhub", "hub": "dockerhub",
    "docker.io": "dockerhub", "hub.docker.com": "dockerhub",
}

DOCKERHUB_API = os.environ.get("DOCKERHUB_API_URL", "https://hub.docker.com")
GITHUB_WEB = os.environ.get("GITHUB_WEB_URL", "https://github.com")
GITHUB_API = os.environ.get("GITHUB_API_URL", "https://api.github.com")
GHCR_REGISTRY = os.environ.get("GHCR_REGISTRY_URL", "https://ghcr.io")
TAG = os.environ.get("TAG", "latest")                # tag used for the size metric
PLATFORM = os.environ.get("PLATFORM", "linux/amd64")  # platform used for multi-arch images


# --------------------------------------------------------------------------- #
# Spec parsing
# --------------------------------------------------------------------------- #
class Image:
    def __init__(self, registry, owner, name):
        self.registry, self.owner, self.name = registry, owner, name

    @property
    def key(self):
        return (self.registry, self.owner, self.name)

    def __str__(self):
        return "%s:%s/%s" % (self.registry, self.owner, self.name)


def parse_spec(spec):
    if ":" not in spec:
        raise ValueError("expected <registry>:<owner>/<image>, got %r" % spec)
    reg, _, path = spec.partition(":")
    registry = REGISTRY_ALIASES.get(reg.strip().lower())
    if not registry:
        raise ValueError("unknown registry %r in %r (use ghcr or dockerhub)" % (reg, spec))
    path = path.strip().strip("/")
    if "/" not in path:
        if registry == "dockerhub":
            path = "library/" + path  # official images
        else:
            raise ValueError("ghcr images need owner/image, got %r" % spec)
    owner, _, name = path.partition("/")
    if not owner or not name:
        raise ValueError("bad image spec %r" % spec)
    return Image(registry, owner, name)


def parse_specs(text):
    out, seen = [], set()
    for tok in re.split(r"[,\s]+", text or ""):
        if not tok:
            continue
        img = parse_spec(tok)
        if img.key not in seen:
            seen.add(img.key)
            out.append(img)
    return out


# --------------------------------------------------------------------------- #
# HTTP helper
# --------------------------------------------------------------------------- #
def http_get(url, headers=None, timeout=20):
    req = urllib.request.Request(url, headers={"User-Agent": UA, **(headers or {})})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def iso_to_epoch(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


# --------------------------------------------------------------------------- #
# Fetchers: each returns {metric_name: value}
# --------------------------------------------------------------------------- #
def fetch_dockerhub(img):
    headers = {"Accept": "application/json"}
    user, tok = os.environ.get("DOCKERHUB_USER"), os.environ.get("DOCKERHUB_TOKEN")
    if user and tok:
        headers["Authorization"] = "Basic " + base64.b64encode(f"{user}:{tok}".encode()).decode()
    url = "%s/v2/repositories/%s/%s/" % (
        DOCKERHUB_API, urllib.parse.quote(img.owner), urllib.parse.quote(img.name))
    d = json.loads(http_get(url, headers))
    m = {
        "registry_image_pull_count": d.get("pull_count"),
        "registry_image_stars": d.get("star_count"),
        "registry_image_last_updated_timestamp_seconds": iso_to_epoch(d.get("last_updated")),
        "registry_image_created_timestamp_seconds": iso_to_epoch(d.get("date_registered")),
    }
    try:
        m["registry_image_size_bytes"] = dockerhub_tag_size(img, headers)
    except Exception as e:  # size is a bonus; don't fail the image over it
        log.warning("size for %s failed: %s", img, e)
    return {k: v for k, v in m.items() if v is not None}


def dockerhub_tag_size(img, headers):
    url = "%s/v2/repositories/%s/%s/tags/%s" % (
        DOCKERHUB_API, urllib.parse.quote(img.owner), urllib.parse.quote(img.name), urllib.parse.quote(TAG))
    d = json.loads(http_get(url, headers))
    os_, arch = (PLATFORM.split("/") + [""])[:2]
    images = d.get("images") or []
    for i in images:
        if i.get("os") == os_ and i.get("architecture") == arch and i.get("size"):
            return i["size"]
    return None if images else d.get("full_size")


_DL_RE = re.compile(r"Total downloads</span>\s*<h3[^>]*title=\"(\d+)\"", re.S)
_PUB_RE = re.compile(r"Last published\s*</span>\s*<h3[^>]*title=\"([^\"]+)\"", re.S)


def fetch_ghcr(img):
    """GitHub has no public API for download counts, so scrape the package page
    (same approach as d0ugal/ghcr-exporter). Packages live under /orgs/ or /users/."""
    html, last_err = None, None
    for kind in ("orgs", "users"):
        url = "%s/%s/%s/packages/container/package/%s" % (
            GITHUB_WEB, kind, urllib.parse.quote(img.owner), urllib.parse.quote(img.name, safe=""))
        try:
            html = http_get(url)
            break
        except urllib.error.HTTPError as e:
            last_err = e
            if e.code != 404:
                raise
    if html is None:
        raise last_err

    m = {}
    dl = _DL_RE.search(html)
    if not dl:
        raise RuntimeError("download count not found on page (GitHub markup changed?)")
    m["registry_image_pull_count"] = int(dl.group(1))
    pub = _PUB_RE.search(html)
    if pub and iso_to_epoch(pub.group(1)):
        m["registry_image_last_updated_timestamp_seconds"] = iso_to_epoch(pub.group(1))

    try:
        m["registry_image_size_bytes"] = ghcr_image_size(img)
    except Exception as e:
        log.warning("size for %s failed: %s", img, e)

    token = os.environ.get("GITHUB_TOKEN")
    if token:  # optional extra: number of versions (tags/digests)
        try:
            m["registry_image_version_count"] = ghcr_version_count(img, token)
        except Exception as e:  # don't fail the whole image over the optional bit
            log.warning("version count for %s failed: %s", img, e)
    return m


MANIFEST_ACCEPT = ",".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])


def ghcr_image_size(img):
    """Compressed size (config + layers) of TAG for PLATFORM, via the OCI registry API."""
    repo = "%s/%s" % (img.owner, img.name)
    scope = urllib.parse.quote("repository:%s:pull" % repo, safe="")
    tok = json.loads(http_get("%s/token?service=ghcr.io&scope=%s" % (GHCR_REGISTRY, scope)))["token"]
    h = {"Authorization": "Bearer " + tok, "Accept": MANIFEST_ACCEPT}
    base = "%s/v2/%s/manifests/" % (GHCR_REGISTRY, urllib.parse.quote(repo, safe="/"))
    man = json.loads(http_get(base + urllib.parse.quote(TAG), h))
    if "manifests" in man:  # multi-arch index: pick our platform (skips attestation entries)
        os_, arch = (PLATFORM.split("/") + [""])[:2]
        for e in man["manifests"]:
            p = e.get("platform") or {}
            if p.get("os") == os_ and p.get("architecture") == arch:
                man = json.loads(http_get(base + e["digest"], h))
                break
        else:
            raise RuntimeError("no %s manifest in %s" % (PLATFORM, TAG))
    return man.get("config", {}).get("size", 0) + sum(l.get("size", 0) for l in man.get("layers", []))


def ghcr_version_count(img, token):
    headers = {"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
               "X-GitHub-Api-Version": "2022-11-28"}
    name = urllib.parse.quote(img.name, safe="")
    for kind in ("orgs", "users"):
        total, page = 0, 1
        try:
            while True:
                url = "%s/%s/%s/packages/container/%s/versions?per_page=100&page=%d" % (
                    GITHUB_API, kind, img.owner, name, page)
                batch = json.loads(http_get(url, headers))
                total += len(batch)
                if len(batch) < 100:
                    return total
                page += 1
        except urllib.error.HTTPError as e:
            if e.code != 404:
                raise
    raise RuntimeError("package not found via API")


FETCHERS = {"dockerhub": fetch_dockerhub, "ghcr": fetch_ghcr}


# --------------------------------------------------------------------------- #
# State + collector
# --------------------------------------------------------------------------- #
class State:
    def __init__(self, images):
        self.images = images
        self.lock = threading.Lock()
        self.data = {}  # key -> dict(metrics, up, ts, duration, error)

    def poll_once(self):
        for img in self.images:
            t0 = time.time()
            try:
                metrics = FETCHERS[img.registry](img)
                res = {"metrics": metrics, "up": 1, "error": ""}
                log.info("%s ok: %s", img, metrics.get("registry_image_pull_count"))
            except Exception as e:
                res = {"up": 0, "error": str(e)}
                log.warning("%s failed: %s", img, e)
            res["ts"], res["duration"] = time.time(), time.time() - t0
            with self.lock:
                old = self.data.get(img.key, {})
                # keep last good values around if this poll failed
                res["metrics"] = res.get("metrics") or old.get("metrics", {})
                self.data[img.key] = res
            time.sleep(0.5)  # be polite between requests

    def loop(self, interval):
        while True:
            self.poll_once()
            time.sleep(interval)


HELP = {
    "registry_image_pull_count": ("Total pulls/downloads of the image.", "gauge"),
    "registry_image_size_bytes": ("Compressed size of the TAG image (default latest) for PLATFORM.", "gauge"),
    "registry_image_stars": ("Docker Hub star count.", "gauge"),
    "registry_image_version_count": ("Number of package versions (ghcr, needs GITHUB_TOKEN).", "gauge"),
    "registry_image_last_updated_timestamp_seconds": ("Unix time the image was last pushed/published.", "gauge"),
    "registry_image_created_timestamp_seconds": ("Unix time the repository was created.", "gauge"),
    "registry_image_up": ("1 if the last poll of this image succeeded.", "gauge"),
    "registry_image_last_poll_timestamp_seconds": ("Unix time of the last poll attempt.", "gauge"),
    "registry_image_poll_duration_seconds": ("Duration of the last poll for this image.", "gauge"),
}


def esc(v):
    return str(v).replace("\\", "\\\\").replace("\n", "\\n").replace('"', '\\"')


def labels(img):
    return 'image="%s"' % esc(img)  # single label, same format as the input: registry:owner/name


def render_metrics(state):
    with state.lock:
        data = dict(state.data)
    rows = {k: [] for k in HELP}
    for img in state.images:
        d = data.get(img.key)
        if not d:
            continue
        lb = labels(img)
        for name, val in d["metrics"].items():
            rows.setdefault(name, []).append("%s{%s} %s" % (name, lb, repr(float(val)) if isinstance(val, float) else val))
        rows["registry_image_up"].append("registry_image_up{%s} %d" % (lb, d["up"]))
        rows["registry_image_last_poll_timestamp_seconds"].append(
            "registry_image_last_poll_timestamp_seconds{%s} %.0f" % (lb, d["ts"]))
        rows["registry_image_poll_duration_seconds"].append(
            "registry_image_poll_duration_seconds{%s} %.3f" % (lb, d["duration"]))

    out = ["# HELP multiregistry_exporter_info Exporter build info.",
           "# TYPE multiregistry_exporter_info gauge",
           'multiregistry_exporter_info{version="%s"} 1' % VERSION]
    for name, lines in rows.items():
        if not lines:
            continue
        help_, typ = HELP.get(name, (name, "gauge"))
        out += ["", "# HELP %s %s" % (name, help_), "# TYPE %s %s" % (name, typ)] + lines
    return "\n".join(out) + "\n"


def render_index(state):
    with state.lock:
        data = dict(state.data)
    rows = []
    for img in state.images:
        d = data.get(img.key)
        if not d:
            rows.append("<tr><td>%s</td><td colspan=4>waiting for first poll…</td></tr>" % img)
            continue
        pulls = d["metrics"].get("registry_image_pull_count")
        upd = d["metrics"].get("registry_image_last_updated_timestamp_seconds")
        rows.append("<tr><td>%s</td><td>%s</td><td>%s</td><td>%s</td><td>%s</td></tr>" % (
            esc(img),
            "&#9989;" if d["up"] else "&#10060;",
            "{:,}".format(pulls) if pulls is not None else "–",
            datetime.fromtimestamp(upd, timezone.utc).strftime("%Y-%m-%d %H:%M UTC") if upd else "–",
            esc(d["error"]) if d["error"] else ""))
    return ("<!doctype html><meta charset=utf-8><title>multiregistry-exporter</title>"
            "<style>body{font:14px system-ui;margin:2em}td,th{padding:4px 12px;text-align:left}"
            "th{border-bottom:1px solid #888}</style>"
            "<h2>multiregistry-exporter %s</h2><p><a href=/metrics>/metrics</a> · <a href=/healthz>/healthz</a></p>"
            "<table><tr><th>image<th>ok<th>pulls<th>last updated<th>error</tr>%s</table>"
            % (VERSION, "".join(rows)))


def make_handler(state):
    class H(BaseHTTPRequestHandler):
        def _send(self, code, body, ctype):
            b = body.encode()
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def do_GET(self):
            p = self.path.split("?")[0]
            if p == "/metrics":
                self._send(200, render_metrics(state), "text/plain; version=0.0.4; charset=utf-8")
            elif p == "/healthz":
                self._send(200, "ok\n", "text/plain")
            elif p == "/":
                self._send(200, render_index(state), "text/html; charset=utf-8")
            else:
                self._send(404, "not found\n", "text/plain")

        def log_message(self, *a):
            pass
    return H


def main():
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO").upper(),
                        format="%(asctime)s %(levelname)s %(message)s")
    spec_text = " ".join(sys.argv[1:]) or os.environ.get("IMAGES", "")
    try:
        images = parse_specs(spec_text)
    except ValueError as e:
        sys.exit("error: %s" % e)
    if not images:
        sys.exit("error: no images configured. Set IMAGES=ghcr:linuxserver/sonarr,dockerhub:jellyfin/jellyfin")

    state = State(images)
    threading.Thread(target=state.loop, args=(int(os.environ.get("INTERVAL", "1800")),), daemon=True).start()
    port = int(os.environ.get("PORT", "9101"))
    log.info("tracking %d image(s); listening on :%d", len(images), port)
    ThreadingHTTPServer(("0.0.0.0", port), make_handler(state)).serve_forever()


if __name__ == "__main__":
    main()
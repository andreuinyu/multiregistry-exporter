FROM python:3.15-alpine

# Injected at build time, e.g. --build-arg VERSION=v1.2.3 (CI passes the git tag)
ARG VERSION=dev
ENV APP_VERSION=${VERSION} \
    PYTHONUNBUFFERED=1
LABEL org.opencontainers.image.version=${VERSION}

WORKDIR /app
COPY app.py .
USER nobody
EXPOSE 9101
HEALTHCHECK --interval=60s --timeout=5s CMD wget -qO- http://127.0.0.1:9101/healthz || exit 1
ENTRYPOINT ["python", "/app/app.py"]
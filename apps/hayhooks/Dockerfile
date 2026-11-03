# syntax=docker/dockerfile:1
#
# The cognition runtime image: Hayhooks serving the Haystack pipelines under `pipelines/`.
# Built from the hash-locked `requirements.txt`, so one image tag always serves the same
# pipeline code over the same dependencies. The stock image's Haystack version is whatever
# PyPI served at its build time.
#
# The build context is the repository root (like every app), so `COPY` paths are
# context-relative. Build it with the generic per-app step (`just podman-build`):
#
#   podman build -f apps/hayhooks/Dockerfile -t localhost/ekg-hayhooks:local .
#
# Python 3.12, because `just hayhooks-lock` compiles requirements.txt for it.
# The runtime runs from the venv at /opt/venv.

FROM docker.io/library/python:3.12-slim-bookworm

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_ROOT_USER_ACTION=ignore

RUN python -m venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"

# Install only locked packages, hashes verified, wheels only (no compiler, no build
# step that could fetch anything unpinned).
COPY apps/hayhooks/requirements.txt /tmp/requirements.txt
RUN pip install --no-deps --require-hashes --only-binary=:all: -r /tmp/requirements.txt \
    && rm -f /tmp/requirements.txt

# The pipelines are part of the image: no bind mount, so the tag is the versioned unit.
# `common/` holds code the pipelines share (provider-failure transparency).
COPY apps/hayhooks/pipelines /pipelines
COPY apps/hayhooks/common /opt/ekg-common
ENV HAYHOOKS_PIPELINES_DIR=/pipelines \
    HAYHOOKS_ADDITIONAL_PYTHON_PATH=/opt/ekg-common

# Haystack's anonymous usage telemetry stays off: the runtime sends nothing anywhere
# but the configured provider (and an OTLP endpoint, only when one is configured).
ENV HAYSTACK_TELEMETRY_ENABLED=False

# A home directory, because Haystack keeps its local config under ~/.haystack.
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin hayhooks
USER hayhooks

EXPOSE 1416
CMD ["hayhooks", "run", "--host", "0.0.0.0"]

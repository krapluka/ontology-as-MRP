# The Plant Pulse frontend as its own service: a static nginx server that packages the
# prebuilt bundle (apps/frontend/dist) and reverse-proxies /api to the backend over the
# network. The app and the API share one origin, so the client keeps calling relative /api
# and needs no CORS. The Rust backend never serves these assets.
#
# The bundle is built on the host or in CI (`npm run build`, e.g. via `just frontend-build`,
# which `just podman-local-up` runs before `just podman-build`), not in this image, so the
# image carries no Node toolchain and needs no package-registry access at build time.
#
# Uses the unprivileged nginx image (uid 101, port 8080), pinned by its multi-arch index
# digest so every build gets the same bytes. Tag 1.30.5-alpine (= stable-alpine, Alpine
# 3.24) at the time of pinning; bump tag and digest together.
FROM docker.io/nginxinc/nginx-unprivileged:1.30.5-alpine@sha256:ed04ec1ff34502c339ee5c3ae3f855442398edc1d05591e2b98981dcbbd20b1e
# The backend API upstream nginx proxies /api to; overridable at run time. Must be
# scheme://host[:port] with no path (checked at start). Default is the backend's
# compose-network DNS name (see deploy/podman/local/compose.yaml).
ENV EKG_API_UPSTREAM=http://ekg-backend:8090
# Substitute only our two variables into the template, so no unrelated environment variable
# can rewrite an nginx runtime variable ($host, $uri, $api).
ENV NGINX_ENVSUBST_FILTER="^(EKG_API_UPSTREAM|NGINX_RESOLVER)$"
# Sourced by the stock entrypoint before 20-envsubst-on-templates.sh: derives NGINX_RESOLVER
# (the container's first nameserver unless set) and validates EKG_API_UPSTREAM.
COPY --chmod=0755 apps/frontend/nginx.proxy-env.envsh /docker-entrypoint.d/16-ekg-proxy-env.envsh
COPY --chmod=0644 apps/frontend/nginx.security-headers.conf /etc/nginx/snippets/security-headers.conf
COPY --chmod=0644 apps/frontend/nginx.default.conf.template /etc/nginx/templates/default.conf.template
COPY apps/frontend/dist /usr/share/nginx/html
USER 101
EXPOSE 8080

#!/usr/bin/env bash
#
# Generate a cross-ecosystem SBOM (Cargo, npm from apps/frontend's committed
# package-lock.json, and pip from apps/hayhooks' hash-locked requirements.txt) with
# resolved licenses, organized by crate/app/service, using ORT (OSS Review Toolkit) run
# inside its pinned container image, so no host JVM is needed. Invoked by `just sbom`.
# Non-gating: cargo-deny stays the supply-chain gate; this is transparency only. See
# docs/sbom/README.md.
#
# Output layout (docs/sbom/): report.html + README.md are committed;
# everything else is generated and gitignored:
#   docs/sbom/bom.cyclonedx.{json,xml}        consolidated CycloneDX
#   docs/sbom/*.spdx.{yml,json}               consolidated SPDX
#   docs/sbom/{crates,apps,services}/<name>/  per-project BOM + license summary
#   docs/sbom/.ort/                           ORT working dir (analyzer + raw reports)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# ort-minimal (~3 GB) carries the commonly-used package managers incl. Cargo;
# override ORT_IMAGE=ghcr.io/oss-review-toolkit/ort for the full (~7 GB) image.
ORT_IMAGE="${ORT_IMAGE:-ghcr.io/oss-review-toolkit/ort-minimal}"
ORT_VERSION="${ORT_VERSION:-84.0.0}"
IMAGE="${ORT_IMAGE}:${ORT_VERSION}"
OUT="docs/sbom"
WORK="${OUT}/.ort"
PERPROJ_RAW="${WORK}/per-project"

# Pick a container runtime, podman preferred.
if command -v podman >/dev/null 2>&1; then
  runtime=podman
elif command -v docker >/dev/null 2>&1; then
  runtime=docker
else
  echo "✗ need podman or docker to run ORT (${IMAGE})" >&2
  exit 1
fi
command -v jq >/dev/null 2>&1 || { echo "✗ missing required tool: jq" >&2; exit 1; }

echo "${IMAGE} via ${runtime}; output in ${OUT}/"

# Clean prior generated output, preserving only the committed files.
rm -rf "${WORK}"
find "${OUT}" -mindepth 1 -maxdepth 1 \
  ! -name 'report.html' ! -name 'README.md' \
  -exec rm -rf {} +
mkdir -p "${WORK}"

# ORT runs as its image's user (uid 1000) and writes into the mounted repository. Rootless
# podman maps that uid to one that cannot, so map the invoking user onto it.
userns=()
if [ "$runtime" = podman ]; then
  userns=(--userns=keep-id:uid=1000,gid=1000)
fi

run_ort() {
  "$runtime" run --rm "${userns[@]}" -v "${repo_root}":/project "${IMAGE}" \
    -c /project/.ort/config.yml "$@"
}

# 1. Analyze the whole workspace (all enabled package managers). The analyzer
#    treats each Cargo crate/app and each npm package as a distinct project keyed
#    by its definition_file_path.
echo "analyze"
run_ort analyze -i /project -o "/project/${WORK}"

analyzer_result="${WORK}/analyzer-result.yml"

# 2. Consolidated reports: CycloneDX + SPDX + WebApp HTML (the human-readable one).
echo "report (consolidated: CycloneDx, SpdxDocument, WebApp)"
run_ort report -i "/project/${analyzer_result}" -o "/project/${OUT}" \
  -f CycloneDx,SpdxDocument,WebApp

# 3. Per-project CycloneDX: one BOM per workspace member (single.bom=false).
echo "report (per-project CycloneDx)"
run_ort report -i "/project/${analyzer_result}" -o "/project/${PERPROJ_RAW}" \
  -f CycloneDx -O CycloneDx=single.bom=false

# 4. Publish the human-readable report to the stable committed path.
webapp="$(find "${OUT}" -maxdepth 1 -name 'scan-report-web-app.html' | head -n1 || true)"
if [ -n "${webapp}" ]; then
  mv -f "${webapp}" "${OUT}/report.html"
  echo "report.html updated"
else
  echo "! WebApp report not found, report.html left unchanged" >&2
fi

# 5. File each per-project BOM under crates/apps/services/packages by matching its
#    root component name to the workspace member's directory, and write a flat
#    component+license summary. Covers Rust members (Cargo.toml) and JS/TS members
#    (package.json). Unknown projects fall back to services/. No associative arrays,
#    so it runs on Bash 3.2 (macOS).
member_dir_for() {
  # echo "<tier>/<dir>" for the member whose manifest name matches $1 (full name or
  # its namespace-stripped basename, so scoped npm names like @scope/foo also match).
  local target="$1" tier dir name
  for tier in crates apps services packages; do
    [ -d "$tier" ] || continue
    for dir in "$tier"/*/; do
      name=""
      if [ -f "${dir}Cargo.toml" ]; then
        name="$(grep -m1 '^name[[:space:]]*=' "${dir}Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
      elif [ -f "${dir}package.json" ]; then
        name="$(jq -r '.name // empty' "${dir}package.json" 2>/dev/null || true)"
      fi
      [ -n "$name" ] || continue
      if [ "$name" = "$target" ] || [ "${name##*/}" = "$target" ]; then
        printf '%s/%s\n' "$tier" "$(basename "$dir")"
        return 0
      fi
    done
  done
  return 1
}

if [ -d "${PERPROJ_RAW}" ]; then
  while IFS= read -r bom; do
    pname="$(jq -r '.metadata.component.name // empty' "$bom" 2>/dev/null || true)"
    pname="${pname##*/}"                       # cargo purls carry the name only
    dest_rel="$(member_dir_for "$pname" || echo "services/${pname:-unknown}")"
    dest="${OUT}/${dest_rel}"
    mkdir -p "$dest"
    cp -f "$bom" "${dest}/bom.cyclonedx.json"
    # component <tab> version <tab> licenses: a quick scan per member.
    jq -r '
      ["component","version","licenses"],
      (.components[]? | [
        .name,
        (.version // ""),
        ((.licenses // []) | map(.license.id // .license.name // .expression) | join(", "))
      ]) | @tsv' "$bom" > "${dest}/licenses.tsv" 2>/dev/null || true
  done < <(find "${PERPROJ_RAW}" -name '*.json')
fi

echo "✓ SBOM generated under ${OUT}/ (committed: report.html; the rest is gitignored)"

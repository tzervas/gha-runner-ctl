#!/bin/bash
# Build a distributable release tarball + checksums for gha-runner-ctl.
#
# Usage:
#   bash scripts/dist.sh                 # uses VERSION file
#   bash scripts/dist.sh 0.1.1
#   bash scripts/dist.sh 0.1.1 --upload  # gh release upload to v0.1.1
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VER="${1:-}"
UPLOAD=0
shift || true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --upload) UPLOAD=1; shift ;;
        *) printf 'unknown arg: %s\n' "$1" >&2; exit 2 ;;
    esac
done

if [[ -z "$VER" ]]; then
    VER="$(tr -d '[:space:]' < VERSION)"
fi
VER="${VER#v}"
TAG="v${VER}"
TARGET="${DIST_TARGET:-x86_64-unknown-linux-gnu}"
OUT_DIR="${ROOT}/dist"
STAGE="${OUT_DIR}/gha-runner-ctl-${VER}-${TARGET}"
ARCHIVE="${OUT_DIR}/gha-runner-ctl-${VER}-${TARGET}.tar.gz"

command -v cargo >/dev/null || { echo "cargo required"; exit 1; }

mkdir -p "$OUT_DIR"
rm -rf "$STAGE"
mkdir -p "$STAGE"

echo "Building release binary (${TARGET})…"
# Host triple may already match; avoid rustup target install if same host.
HOST="$(rustc -vV | awk '/^host:/{print $2}')"
if [[ "$HOST" == "$TARGET" ]]; then
    cargo build --release
    BIN="${ROOT}/target/release/gha-runner-ctl"
else
    rustup target add "$TARGET" 2>/dev/null || true
    cargo build --release --target "$TARGET"
    BIN="${ROOT}/target/${TARGET}/release/gha-runner-ctl"
fi

[[ -x "$BIN" ]] || { echo "missing binary: $BIN"; exit 1; }

install -m 755 "$BIN" "${STAGE}/gha-runner-ctl"
cp -a LICENSE NOTICE README.md CHANGELOG.md "${STAGE}/"
mkdir -p "${STAGE}/docs" "${STAGE}/packaging"
cp -a docs/. "${STAGE}/docs/" 2>/dev/null || true
cp -a packaging/Containerfile packaging/entrypoint.sh packaging/install-ctl.sh \
    "${STAGE}/packaging/" 2>/dev/null || true

# Install helper that works from the tarball (no cargo required)
cat >"${STAGE}/install.sh" <<'EOS'
#!/bin/bash
# Install gha-runner-ctl from a release tarball into ~/.local/bin (or --prefix).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${HOME}/.local"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) PREFIX="${2:-}"; shift 2 ;;
        *) echo "usage: $0 [--prefix DIR]"; exit 2 ;;
    esac
done
mkdir -p "${PREFIX}/bin"
install -m 755 "${HERE}/gha-runner-ctl" "${PREFIX}/bin/gha-runner-ctl"
echo "Installed ${PREFIX}/bin/gha-runner-ctl"
command -v gha-runner-ctl >/dev/null 2>&1 || echo "Add to PATH: export PATH=\"${PREFIX}/bin:\$PATH\""
echo "Next: gha-runner-ctl prepare && gha-runner-ctl --help"
EOS
chmod 755 "${STAGE}/install.sh"

tar -C "$OUT_DIR" -czf "$ARCHIVE" "$(basename "$STAGE")"
(
    cd "$OUT_DIR"
    sha256sum "$(basename "$ARCHIVE")" > SHA256SUMS-${VER}.txt
    # Also a stable name for the latest artifact set
    cp -f "SHA256SUMS-${VER}.txt" SHA256SUMS.txt
)

echo "Built:"
echo "  $ARCHIVE"
echo "  ${OUT_DIR}/SHA256SUMS-${VER}.txt"
sha256sum "$ARCHIVE"

if (( UPLOAD == 1 )); then
    command -v gh >/dev/null || { echo "gh required for --upload"; exit 1; }
    if ! gh release view "$TAG" >/dev/null 2>&1; then
        echo "Creating release ${TAG}…"
        gh release create "$TAG" \
            --title "gha-runner-ctl ${TAG}" \
            --notes-file - <<EOF
## gha-runner-ctl ${TAG}

Binary distribution for Linux x86_64.

### Install
\`\`\`bash
curl -fsSL -o gha-runner-ctl.tar.gz \\
  https://github.com/tzervas/gha-runner-ctl/releases/download/${TAG}/gha-runner-ctl-${VER}-${TARGET}.tar.gz
curl -fsSL -o SHA256SUMS.txt \\
  https://github.com/tzervas/gha-runner-ctl/releases/download/${TAG}/SHA256SUMS-${VER}.txt
sha256sum -c SHA256SUMS.txt --ignore-missing
tar xzf gha-runner-ctl.tar.gz
cd gha-runner-ctl-${VER}-${TARGET}
bash install.sh
export PATH=\"\$HOME/.local/bin:\$PATH\"
gha-runner-ctl prepare
\`\`\`

See README and docs/SECURITY.md.
EOF
    fi
    gh release upload "$TAG" \
        "$ARCHIVE" \
        "${OUT_DIR}/SHA256SUMS-${VER}.txt" \
        --clobber
    echo "Uploaded to https://github.com/tzervas/gha-runner-ctl/releases/tag/${TAG}"
fi

#!/usr/bin/env bash
#
# tsm installer — downloads the latest tsm release and installs it locally.
#
#   curl -fsSL https://key.github.io/the-space-memory/install.sh | bash
#
# Environment variables:
#   TSM_VERSION    Specific release tag to install (default: latest)
#   INSTALL_DIR    Where to place binaries     (default: $HOME/.local/bin)
#   TSM_FORCE      Set to "1" to overwrite an existing installation without prompt
set -euo pipefail

readonly REPO="key/the-space-memory"
readonly DEFAULT_INSTALL_DIR="${HOME}/.local/bin"

# Global temp dir, populated by main(). Declared here so the EXIT trap
# (registered before mktemp) can reference it without `set -u` complaints
# even if the script aborts before mktemp succeeds.
TMP_DIR=""
cleanup() { [ -n "${TMP_DIR:-}" ] && rm -rf "${TMP_DIR}"; }
trap cleanup EXIT

#
# Logging helpers
#
info()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
fatal() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

#
# Pre-flight: required commands
#
require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fatal "required command not found: $1"
}
require_cmd curl
require_cmd tar
require_cmd uname
require_cmd install
require_cmd mktemp

# Use shasum if available (macOS), else sha256sum (Linux).
if command -v shasum >/dev/null 2>&1; then
    SHA256_CHECK=(shasum -a 256 -c)
elif command -v sha256sum >/dev/null 2>&1; then
    SHA256_CHECK=(sha256sum -c)
else
    fatal "neither shasum nor sha256sum is available"
fi

#
# Detect platform → archive_name (matches .github/workflows/release.yml matrix)
#
detect_arch() {
    local kernel machine
    kernel=$(uname -s)
    machine=$(uname -m)

    case "${kernel}-${machine}" in
        Linux-x86_64)        echo "linux-x86_64" ;;
        Linux-aarch64|Linux-arm64) echo "linux-arm64" ;;
        Darwin-arm64)        echo "darwin-arm64" ;;
        Darwin-x86_64)
            fatal "Intel Mac is not supported by binary releases. Build from source: https://github.com/${REPO}"
            ;;
        *)
            fatal "unsupported platform: ${kernel} ${machine}"
            ;;
    esac
}

#
# Resolve the release tag — env override or latest from GitHub API.
#
# The curl call is split out so that a non-zero grep exit (no tag_name in the
# response — rate-limited, network outage, schema change) does not abort the
# script via pipefail before the empty-tag guard in main() runs.
#
resolve_tag() {
    if [ -n "${TSM_VERSION:-}" ]; then
        echo "${TSM_VERSION}"
        return
    fi
    local response
    response=$(curl -fsSL --proto '=https' --tlsv1.2 \
        "https://api.github.com/repos/${REPO}/releases/latest") \
        || fatal "failed to reach GitHub API — check your network connection"
    if printf '%s' "${response}" | grep -q '"message"'; then
        local api_msg
        api_msg=$(printf '%s' "${response}" | grep -oE '"message": *"[^"]+"' | head -n1 || true)
        fatal "GitHub API returned an error (${api_msg:-unknown}) — set TSM_VERSION to bypass"
    fi
    printf '%s' "${response}" \
        | grep -oE '"tag_name": *"[^"]+"' \
        | head -n1 \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' \
        || true
}

#
# Download a URL into a path; abort the script on any HTTP failure.
#
fetch() {
    local url="$1" dest="$2"
    curl -fsSL --proto '=https' --tlsv1.2 -o "${dest}" "${url}"
}

#
# Confirm overwrite if either tsm or tsmd already exists at INSTALL_DIR.
# Both binaries are part of one logical installation — skipping the prompt
# when only tsmd is present would silently overwrite a partial install.
#
confirm_overwrite() {
    local install_dir="$1"
    local tsm_path="${install_dir}/tsm"
    local tsmd_path="${install_dir}/tsmd"
    if [ ! -e "${tsm_path}" ] && [ ! -e "${tsmd_path}" ]; then
        return 0
    fi

    # Pick whichever exists for the version banner; prefer tsm.
    local probe="${tsm_path}"
    [ -e "${probe}" ] || probe="${tsmd_path}"
    local current_version
    current_version=$("${probe}" --version 2>/dev/null || echo "unknown")
    warn "existing installation found at ${install_dir} (${current_version})"

    if [ "${TSM_FORCE:-}" = "1" ]; then
        warn "TSM_FORCE=1 — overwriting"
        return 0
    fi

    if [ ! -t 0 ]; then
        fatal "non-interactive shell — re-run with TSM_FORCE=1 to overwrite"
    fi

    # `read -r` can return non-zero on EOF (Ctrl-D) or stream error. Without
    # explicit handling, set -e would exit the script silently.
    local reply=""
    if ! read -r reply; then
        fatal "could not read response — aborting (use TSM_FORCE=1 to bypass)"
    fi
    case "${reply}" in
        [yY]|[yY][eE][sS]) return 0 ;;
        *) fatal "aborted by user" ;;
    esac
}

#
# Print the post-install message — what to run next.
#
print_next_steps() {
    local install_dir="$1"
    cat <<EOF

✓ tsm installed at ${install_dir}

Next steps:
  1. tsm setup         # Download embedding model and WordNet into .tsm/
  2. tsm init          # Initialize this workspace's database
  3. tsm doctor        # Verify everything is in order

Documentation: https://github.com/${REPO}
EOF

    case ":${PATH}:" in
        *":${install_dir}:"*) ;;
        *)
            warn "${install_dir} is not on your PATH"
            cat <<EOF
   Add it to your shell profile:
     echo 'export PATH="${install_dir}:\$PATH"' >> ~/.bashrc   # or ~/.zshrc
     source ~/.bashrc
EOF
            ;;
    esac
}

main() {
    local install_dir="${INSTALL_DIR:-${DEFAULT_INSTALL_DIR}}"
    info "tsm installer"

    local arch tag
    arch=$(detect_arch)
    info "platform: ${arch}"

    tag=$(resolve_tag)
    [ -n "${tag}" ] || fatal "could not resolve release tag (set TSM_VERSION manually)"
    info "version: ${tag}"

    confirm_overwrite "${install_dir}"

    local archive_name="tsm-${tag}-${arch}.tar.gz"
    local archive_url="https://github.com/${REPO}/releases/download/${tag}/${archive_name}"
    local sha_url="${archive_url}.sha256"

    TMP_DIR=$(mktemp -d)

    info "downloading ${archive_name}"
    fetch "${archive_url}" "${TMP_DIR}/${archive_name}"

    info "verifying checksum"
    fetch "${sha_url}" "${TMP_DIR}/${archive_name}.sha256"
    # Don't redirect output — preserve forensic detail (which file failed,
    # expected vs actual hash) for security-relevant failures.
    (cd "${TMP_DIR}" && "${SHA256_CHECK[@]}" "${archive_name}.sha256") \
        || fatal "checksum mismatch for ${archive_name} — the downloaded file may be corrupt or tampered with"

    info "extracting"
    tar -xzf "${TMP_DIR}/${archive_name}" -C "${TMP_DIR}"
    local extracted_dir="${TMP_DIR}/tsm-${tag}-${arch}"
    [ -d "${extracted_dir}" ] \
        || fatal "expected directory ${extracted_dir} not found in archive — release layout may have changed"

    info "installing to ${install_dir}"
    mkdir -p "${install_dir}"
    # Stage both binaries first, then move atomically. Avoids leaving the
    # system in a half-installed state if the second copy fails.
    install -m 755 "${extracted_dir}/bin/tsm"  "${TMP_DIR}/tsm.new"
    install -m 755 "${extracted_dir}/bin/tsmd" "${TMP_DIR}/tsmd.new"
    mv "${TMP_DIR}/tsm.new"  "${install_dir}/tsm"
    mv "${TMP_DIR}/tsmd.new" "${install_dir}/tsmd"

    print_next_steps "${install_dir}"
}

main "$@"

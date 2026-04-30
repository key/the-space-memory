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
#
# See ADR-0019 for the design rationale.
set -euo pipefail

readonly REPO="key/the-space-memory"
readonly DEFAULT_INSTALL_DIR="${HOME}/.local/bin"

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
# Resolve the release tag — env override or latest from GitHub API
#
resolve_tag() {
    if [ -n "${TSM_VERSION:-}" ]; then
        echo "${TSM_VERSION}"
        return
    fi
    # Use the GitHub API. Don't depend on jq.
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -oE '"tag_name": *"[^"]+"' \
        | head -n1 \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/'
}

#
# Download a URL into a path; abort the script on any HTTP failure.
#
fetch() {
    local url="$1" dest="$2"
    curl -fsSL --proto '=https' --tlsv1.2 -o "${dest}" "${url}"
}

#
# Confirm overwrite if a binary already exists at INSTALL_DIR.
#
confirm_overwrite() {
    local install_dir="$1"
    local existing="${install_dir}/tsm"
    if [ ! -e "${existing}" ]; then
        return 0
    fi

    local current_version
    current_version=$("${existing}" --version 2>/dev/null || echo "unknown")
    warn "existing tsm found at ${existing} (${current_version})"

    if [ "${TSM_FORCE:-}" = "1" ]; then
        warn "TSM_FORCE=1 — overwriting"
        return 0
    fi

    if [ ! -t 0 ]; then
        fatal "non-interactive shell — re-run with TSM_FORCE=1 to overwrite"
    fi

    printf "Overwrite? [y/N] "
    read -r reply
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
  1. tsm setup         # Download embedding model and WordNet (one-time, system-wide)
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

    local tmp
    tmp=$(mktemp -d)
    trap 'rm -rf "${tmp}"' EXIT

    info "downloading ${archive_name}"
    fetch "${archive_url}" "${tmp}/${archive_name}"

    info "verifying checksum"
    fetch "${sha_url}" "${tmp}/${archive_name}.sha256"
    (cd "${tmp}" && "${SHA256_CHECK[@]}" "${archive_name}.sha256" >/dev/null) \
        || fatal "checksum mismatch — aborting"

    info "extracting"
    tar -xzf "${tmp}/${archive_name}" -C "${tmp}"
    local extracted_dir="${tmp}/tsm-${tag}-${arch}"

    info "installing to ${install_dir}"
    mkdir -p "${install_dir}"
    install -m 755 "${extracted_dir}/bin/tsm"  "${install_dir}/"
    install -m 755 "${extracted_dir}/bin/tsmd" "${install_dir}/"

    print_next_steps "${install_dir}"
}

main "$@"

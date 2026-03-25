#!/bin/bash
set -euo pipefail

# Docker named volumes are initially owned by root; fix for vscode user
sudo chown -R vscode:vscode /home/vscode/.config  # gh CLI config
sudo chown -R vscode:vscode /home/vscode/.claude   # Claude Code config

# Claude Code skip onboarding
if [ ! -f "$HOME/.claude.json" ]; then
  cat > "$HOME/.claude.json" << 'EOF'
{
  "hasCompletedOnboarding": true,
  "hasAckedPrivacyPolicy": true,
  "completedOnboardingAt": "2026-02-10T00:00:00.000Z",
  "opusProMigrationComplete": true
}
EOF
fi

# Git configuration
if [ -n "${GIT_USERNAME:-}" ]; then
  git config --global user.name "$GIT_USERNAME"
fi
if [ -n "${GIT_EMAIL:-}" ]; then
  git config --global user.email "$GIT_EMAIL"
fi

# Install mise and tools
# Shims are created in ~/.local/share/mise/shims/ during install (on PATH via Dockerfile ENV)
curl -fsSL https://mise.run | sh
mise trust
# Register uv globally first — pipx:yamllint invokes uv from a temp directory,
# so a global version is needed for the shim to resolve correctly.
mise use -g github:astral-sh/uv@latest
mise install

# Install Claude Code (native installer)
curl -fsSL https://claude.ai/install.sh | bash

# Verify all tools are available
echo "--- Tool verification ---"
yamllint --version
rumdl --version
shellcheck --version
taplo --version
gh --version
jq --version

echo "Dev container ready!"

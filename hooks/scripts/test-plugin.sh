#!/usr/bin/env bash
set -euo pipefail

# Test 1: Can we find the binary via CLAUDE_PLUGIN_ROOT?
TSM="${CLAUDE_PLUGIN_ROOT}/tsm"
if [ -x "$TSM" ]; then
  echo "OK: binary found at $TSM" >&2
else
  echo "FAIL: binary not found at $TSM" >&2
  exit 1
fi

# Test 2: Can we run it with tsm.toml from project dir?
cd "${CLAUDE_PROJECT_DIR:-/workspaces/workspace}"
VERSION=$("$TSM" --version 2>&1) || true
echo "OK: $VERSION" >&2

# Test 3: Can doctor find the DB via tsm.toml?
DOCTOR=$("$TSM" doctor 2>&1) || true
echo "$DOCTOR" >&2

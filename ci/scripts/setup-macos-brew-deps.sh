#!/usr/bin/env bash
set -euo pipefail

# Install requested formulae, then build a PKG_CONFIG_PATH that includes
# the Homebrew prefix and selected formulae.
FORMULAE=()
if [[ $# -gt 0 ]]; then
  FORMULAE=("$@")
fi

if [[ ${#FORMULAE[@]} -eq 0 ]]; then
  echo "Usage: $0 <formula> [formula...]" >&2
  exit 2
fi

brew install "${FORMULAE[@]}"

BREW_PREFIX="$(brew --prefix)"
PKG_PATHS=("${BREW_PREFIX}/lib/pkgconfig")

for formula in "${FORMULAE[@]}"; do
  prefix="$(brew --prefix "${formula}" 2>/dev/null || true)"
  if [[ -n "${prefix}" && -d "${prefix}/lib/pkgconfig" ]]; then
    PKG_PATHS+=("${prefix}/lib/pkgconfig")
  fi
done

PKG_CONFIG_PATH="$(IFS=:; echo "${PKG_PATHS[*]}"):${PKG_CONFIG_PATH:-}"
export PKG_CONFIG_PATH

# Persist for GitHub Actions
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "PKG_CONFIG_PATH=${PKG_CONFIG_PATH}" >> "${GITHUB_ENV}"
fi

#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <version>" >&2
  exit 1
fi

version="$1"
root="$(cd "$(dirname "$0")/.." && pwd)"

# Cargo.toml: replace first occurrence of version = "..."
sed -i '0,/^version = ".*"/{s/^version = ".*"/version = "'"$version"'"/}' "$root/Cargo.toml"

# Chart.yaml: version and appVersion
sed -i 's/^version: .*/version: '"$version"'/' "$root/chart/Chart.yaml"
sed -i 's/^appVersion: .*/appVersion: "'"$version"'"/' "$root/chart/Chart.yaml"

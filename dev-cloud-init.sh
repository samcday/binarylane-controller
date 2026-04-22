#!/bin/sh
set -eu
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  SUDO="sudo"
fi
if ! command -v curl >/dev/null 2>&1; then
  if command -v apt-get >/dev/null 2>&1; then
    ${SUDO} apt-get update -y
    ${SUDO} apt-get install -y curl
  elif command -v dnf >/dev/null 2>&1; then
    ${SUDO} dnf install -y curl
  elif command -v apk >/dev/null 2>&1; then
    ${SUDO} apk add --no-cache curl
  fi
fi
if ! command -v k3s >/dev/null 2>&1; then
  curl -sfL https://get.k3s.io | ${SUDO} env K3S_URL="{{ k3sUrl }}" K3S_TOKEN="{{ k3sToken }}" INSTALL_K3S_EXEC="agent --node-name {{ node.name }} --node-label autoscale-group={{ asg.name }}" sh -s -
fi
${SUDO} systemctl enable --now k3s-agent >/dev/null 2>&1 || true

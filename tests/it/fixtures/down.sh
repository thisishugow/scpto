#!/usr/bin/env bash
# Stops and removes the scpto integration-test containers.
set -euo pipefail
docker rm -f scpto-it-modern scpto-it-legacy >/dev/null 2>&1 || true

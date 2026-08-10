#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DRIFT_RS_DIR="$(dirname "$SCRIPT_DIR")"
PROTOCOL_V2_DIR="${PROTOCOL_V2_DIR:-$DRIFT_RS_DIR/../protocol-v2}"

cp "$PROTOCOL_V2_DIR/sdk/src/idl/drift.json" "$DRIFT_RS_DIR/res/drift.json"

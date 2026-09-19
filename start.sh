#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# Optional local configuration (never committed): .env.local
if [ -f .env.local ]; then
  . ./.env.local
fi
: "${TYPESAFE_API_KEY:?TYPESAFE_API_KEY must be set (export it, or create .env.local)}"

exec ./target/release/mcp_typesafe --bind 0.0.0.0:3391 -t sse --typesafe-api-key "${TYPESAFE_API_KEY}"


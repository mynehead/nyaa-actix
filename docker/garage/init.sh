#!/bin/sh
# Prepares the Garage container from compose.yml: gives the single node its storage role,
# then creates the `nyaa` bucket and the dev key from .env.example. Safe to run again.
set -e
garage() { docker compose -f "$(dirname "$0")/../compose.yml" exec -T garage /garage "$@"; }

until garage status >/dev/null 2>&1; do sleep 1; done
NODE_ID=$(garage status | awk '/NO ROLE ASSIGNED/ {print $1; exit}')
if [ -n "$NODE_ID" ]; then
  garage layout assign -z dc1 -c 1G "$NODE_ID" >/dev/null
  garage layout apply --version 1 >/dev/null
fi
garage key import --yes -n nyaa-dev GK0123456789abcdef01234567 \
  0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef >/dev/null 2>&1 || true
garage bucket create nyaa >/dev/null 2>&1 || true
garage bucket allow --read --write --owner nyaa --key nyaa-dev >/dev/null
echo "Garage is ready: bucket nyaa at http://localhost:3900"

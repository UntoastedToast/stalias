#!/usr/bin/env bash
# Starts a throwaway Stalwart 0.16 in Docker and prepares it for the live tests:
# finishes the bootstrap, creates a test mailbox and a minimal API key.
# Progress goes to stderr; stdout is a set of `export` lines, so use it like this:
#
#   eval "$(scripts/setup-test-instance.sh)"
#   cargo test -- --ignored --test-threads=1
set -euo pipefail

NAME=${NAME:-stalias-dev-stalwart}
IMAGE=${IMAGE:-stalwartlabs/stalwart:v0.16.22}
HTTP_PORT=${HTTP_PORT:-8080}
SMTP_PORT=${SMTP_PORT:-2525}
URL="http://127.0.0.1:${HTTP_PORT}"
USING='"using":["urn:ietf:params:jmap:core","urn:stalwart:jmap"]'

log() { echo "$@" >&2; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
jmap() { curl -fsS -u "$CREDS" -H 'Content-Type: application/json' "${URL}/jmap/" -d "{${USING},\"methodCalls\":[$1]}"; }
wait_for() { # wait_for <description> <command...>
  local what=$1; shift
  for _ in $(seq 60); do "$@" >/dev/null 2>&1 && return 0; sleep 1; done
  log "timed out waiting for $what"; exit 1
}

docker rm -f "$NAME" >/dev/null 2>&1 || true
log "Starting $IMAGE as $NAME"
docker run -d --name "$NAME" --hostname mail.test.local \
  -p "127.0.0.1:${HTTP_PORT}:8080" -p "127.0.0.1:${SMTP_PORT}:25" "$IMAGE" >/dev/null

wait_for "bootstrap password" bash -c "docker logs $NAME 2>&1 | grep -q 'password:'"
BOOT_PASS=$(docker logs "$NAME" 2>&1 | sed -n 's/.*password: //p' | head -1)
CREDS="admin:${BOOT_PASS}"
wait_for "Stalwart" curl -fsu "$CREDS" "${URL}/jmap/session"

log "Finishing bootstrap"
RESULT=$(jmap '["x:Bootstrap/set",{"accountId":"d333333","update":{"singleton":{"requestTlsCertificate":false}}},"b"]')
CREDS=$(echo "$RESULT" | json 'd["methodResponses"][0][1]["updated"]["singleton"]["username"]+":"+d["methodResponses"][0][1]["updated"]["singleton"]["secret"]')
docker restart "$NAME" >/dev/null
wait_for "Stalwart restart" curl -fsu "$CREDS" "${URL}/jmap/session"

log "Creating test mailbox and API key"
DOMAIN_ID=$(jmap '["x:Domain/get",{"ids":null,"properties":["name"]},"d"]' | json 'd["methodResponses"][0][1]["list"][0]["id"]')
ACCOUNT_ID=$(jmap '["x:Account/set",{"create":{"n":{"@type":"User","name":"alice","domainId":"'"$DOMAIN_ID"'","credentials":{"0":{"@type":"Password","secret":"alice-test-password"}}}}},"c"]' \
  | json 'd["methodResponses"][0][1]["created"]["n"]["id"]')
KEY=$(jmap '["x:ApiKey/set",{"create":{"k":{"description":"stalias","allowedIps":{},"permissions":{"@type":"Replace","permissions":{"authenticate":true,"sysAccountGet":true,"sysAccountQuery":true,"sysAccountUpdate":true,"sysDomainGet":true,"sysDomainQuery":true}}}}},"k"]' \
  | json 'd["methodResponses"][0][1]["created"]["k"]["secret"]')

log "Ready. Stalwart admin UI: ${URL}/admin  login ${CREDS%%:*}  password ${CREDS#*:}"
log "Test mailbox: alice@test.local"
cat <<OUT
export STALIAS_TEST_URL=${URL}
export STALIAS_TEST_KEY=${KEY}
export STALIAS_TEST_ACCOUNT=${ACCOUNT_ID}
export STALIAS_TEST_DOMAIN=test.local
OUT

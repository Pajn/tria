# Runs inside the remote login shell. stdout contains only a framed result.
set -eu
umask 077
TRIA_DATA="${T3CODE_HOME:-$HOME/.t3}"
mkdir -p "$TRIA_DATA/userdata"
# Common user installs are not always in a non-interactive login shell's PATH.
PATH="$HOME/.local/bin:$HOME/.npm-packages/bin:$HOME/.bun/bin:$PATH"
export PATH
TRIA_RUNNER="$(command -v t3 || true)"
if [ -z "$TRIA_RUNNER" ] && [ -f "$TRIA_DATA/runtime/tria-runner" ]; then
  TRIA_CACHED="$(cat "$TRIA_DATA/runtime/tria-runner")"
  if [ -x "$TRIA_CACHED" ]; then TRIA_RUNNER="$TRIA_CACHED"; fi
fi
if [ -z "$TRIA_RUNNER" ]; then
  # Install an official standalone release; provider CLIs remain the host's setup.
  tria_fetch() {
    if command -v curl >/dev/null 2>&1; then
      curl -fsSL --connect-timeout 15 --max-time 120 "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
      wget -q --timeout=30 --tries=1 "$1" -O "$2"
    else
      printf 'Install t3, or curl/wget to download its standalone release.\n' >&2
      exit 1
    fi
  }
  TRIA_STAGE="$(mktemp -d "$TRIA_DATA/tria-install.XXXXXX")"
  trap 'rm -rf "$TRIA_STAGE"' EXIT HUP INT TERM
  tria_fetch 'https://api.github.com/repos/pingdotgg/t3code/releases/latest' "$TRIA_STAGE/release.json"
  TRIA_VERSION="$(sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' "$TRIA_STAGE/release.json" | head -1)"
  case "$TRIA_VERSION" in ''|*[!0-9A-Za-z.-]*) printf 'Invalid t3 release version.\n' >&2; exit 1;; esac
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64|Linux/amd64) TRIA_PLATFORM=linux-x64;;
    Linux/aarch64|Linux/arm64) TRIA_PLATFORM=linux-arm64;;
    Darwin/arm64) TRIA_PLATFORM=darwin-arm64;;
    *) printf 'Standalone t3 requires Linux or an Apple Silicon Mac.\n' >&2; exit 1;;
  esac
  TRIA_ARCHIVE="t3-$TRIA_VERSION-$TRIA_PLATFORM.tar.gz"
  TRIA_RELEASE="https://github.com/pingdotgg/t3code/releases/download/v$TRIA_VERSION"
  tria_fetch "$TRIA_RELEASE/SHA256SUMS" "$TRIA_STAGE/SHA256SUMS"
  tria_fetch "$TRIA_RELEASE/$TRIA_ARCHIVE" "$TRIA_STAGE/archive.tar.gz"
  TRIA_EXPECTED="$(awk -v archive="$TRIA_ARCHIVE" '{name=$2; sub(/^\*/, "", name); if (name==archive) print $1}' "$TRIA_STAGE/SHA256SUMS")"
  if command -v sha256sum >/dev/null 2>&1; then
    TRIA_ACTUAL="$(sha256sum "$TRIA_STAGE/archive.tar.gz" | cut -d' ' -f1)"
  else
    TRIA_ACTUAL="$(shasum -a 256 "$TRIA_STAGE/archive.tar.gz" | cut -d' ' -f1)"
  fi
  if [ -z "$TRIA_EXPECTED" ] || [ "$TRIA_EXPECTED" != "$TRIA_ACTUAL" ]; then
    printf 't3 archive checksum mismatch.\n' >&2; exit 1
  fi
  mkdir "$TRIA_STAGE/runtime"
  tar -xzf "$TRIA_STAGE/archive.tar.gz" -C "$TRIA_STAGE/runtime" --strip-components=1
  "$TRIA_STAGE/runtime/t3" --version >/dev/null
  # Each install has its own directory: concurrent clients never replace one another.
  mkdir -p "$TRIA_DATA/runtime"
  TRIA_INSTALLED="$(mktemp -d "$TRIA_DATA/runtime/tria-$TRIA_VERSION.XXXXXX")"
  mv "$TRIA_STAGE/runtime" "$TRIA_INSTALLED/server"
  TRIA_RUNNER="$TRIA_INSTALLED/server/t3"
  printf '%s\n' "$TRIA_RUNNER" > "$TRIA_INSTALLED/runner-path"
  mv "$TRIA_INSTALLED/runner-path" "$TRIA_DATA/runtime/tria-runner"
  rm -rf "$TRIA_STAGE"
  trap - EXIT HUP INT TERM
fi
# Serialize discovery/start on this T3 home, including across different SSH clients.
TRIA_LOCK="$TRIA_DATA/userdata/tria-start.lock"
TRIA_WAIT=0
while ! mkdir "$TRIA_LOCK" 2>/dev/null; do
  TRIA_OWNER="$(cat "$TRIA_LOCK/pid" 2>/dev/null || true)"
  if [ -n "$TRIA_OWNER" ] && ! kill -0 "$TRIA_OWNER" 2>/dev/null; then
    rm -rf "$TRIA_LOCK"
    continue
  fi
  TRIA_WAIT=$((TRIA_WAIT + 1))
  if [ "$TRIA_WAIT" -ge 45 ]; then
    printf 'Another client is starting t3; retry shortly.\n' >&2; exit 1
  fi
  sleep 1
done
printf '%s\n' "$$" > "$TRIA_LOCK/pid"
trap 'rm -rf "$TRIA_LOCK"' EXIT HUP INT TERM
TRIA_RUNTIME="$TRIA_DATA/userdata/server-runtime.json"
TRIA_INFO="$("$TRIA_RUNNER" __ssh-helper runtime-port "$TRIA_RUNTIME" 2>/dev/null || true)"
TRIA_PORT="${TRIA_INFO##* }"
TRIA_FORWARD_HOST=127.0.0.1
case "$TRIA_PORT" in ''|*[!0-9]*) TRIA_PORT='';; esac
# The helper only discovers loopback servers. A LAN-bound live server still owns
# this database; forward to its bind address rather than start a second writer.
if [ -z "$TRIA_PORT" ] && [ -f "$TRIA_RUNTIME" ]; then
  TRIA_PID="$(sed -n 's/.*"pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$TRIA_RUNTIME" | head -1)"
  if [ -n "$TRIA_PID" ] && kill -0 "$TRIA_PID" 2>/dev/null; then
    TRIA_PORT="$(sed -n 's/.*"port":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$TRIA_RUNTIME" | head -1)"
    TRIA_FORWARD_HOST="$(sed -n 's/.*"host":[[:space:]]*"\([^"]*\)".*/\1/p' "$TRIA_RUNTIME" | head -1)"
    case "$TRIA_FORWARD_HOST" in ''|0.0.0.0) TRIA_FORWARD_HOST=127.0.0.1;; '::') TRIA_FORWARD_HOST='::1';; esac
    case "$TRIA_PORT/$TRIA_FORWARD_HOST" in *[!0-9A-Za-z.:/-]*|'/'*) printf 'Invalid live t3 runtime endpoint.\n' >&2; exit 1;; esac
    if [ -z "$TRIA_PORT" ]; then printf 'Live t3 runtime has no port; refusing to start a second server.\n' >&2; exit 1; fi
  fi
fi
if [ -z "$TRIA_PORT" ]; then
  TRIA_PORT="$("$TRIA_RUNNER" __ssh-helper pick-port "$TRIA_DATA/userdata/tria-port" 3773 100)"
  nohup env T3CODE_NO_BROWSER=1 "$TRIA_RUNNER" serve --host 127.0.0.1 --port "$TRIA_PORT" --base-dir "$TRIA_DATA" >>"$TRIA_DATA/userdata/tria-server.log" 2>&1 </dev/null &
  if ! "$TRIA_RUNNER" __ssh-helper wait-ready "$TRIA_PORT" 30000 1000 >/dev/null 2>&1; then
    printf 't3 failed to start; see %s/userdata/tria-server.log on the host.\n' "$TRIA_DATA" >&2
    exit 1
  fi
fi
TRIA_TOKEN="$("$TRIA_RUNNER" auth session issue --base-dir "$TRIA_DATA" --label tria --ttl 30d --token-only)"
printf 'TRIA_SERVER\000%s\000%s\000%s\000' "$TRIA_PORT" "$TRIA_TOKEN" "$TRIA_FORWARD_HOST"

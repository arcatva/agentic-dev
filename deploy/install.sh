#!/bin/sh
# agentic-dev installer — run from an unpacked release tarball (or repo deploy/ after `make build`).
#
#   ./install.sh              install/upgrade + start the service (systemd --user / launchd)
#   ./install.sh --uninstall  stop the service and remove installed files (keeps ~/.agentic-dev data)
#
# Layout it installs:
#   ~/.local/share/agentic-dev/   agentic-dev-server + sdk-bridge.mjs + node_modules (bridge deps)
#   ~/.agentic-dev/service.env    generated secrets (AGENTIC_PASSWORD / AGENTIC_AUTH_SECRET), mode 600
#   systemd: ~/.config/systemd/user/agentic-dev.service
#   launchd: ~/Library/LaunchAgents/dev.agentic.server.plist
#
# Override the install dir with AGENTIC_INSTALL_DIR (the service templates assume the default).
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
INSTALL_DIR="${AGENTIC_INSTALL_DIR:-$HOME/.local/share/agentic-dev}"
DATA_DIR="$HOME/.agentic-dev"
ENV_FILE="$DATA_DIR/service.env"
OS=$(uname -s)

info() { printf '==> %s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

uninstall() {
    if [ "$OS" = "Darwin" ]; then
        launchctl bootout "gui/$(id -u)/dev.agentic.server" 2>/dev/null || true
        rm -f "$HOME/Library/LaunchAgents/dev.agentic.server.plist"
    elif command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now agentic-dev 2>/dev/null || true
        rm -f "$HOME/.config/systemd/user/agentic-dev.service"
        systemctl --user daemon-reload 2>/dev/null || true
    fi
    rm -rf "$INSTALL_DIR"
    info "uninstalled. Data (sqlite, logs, secrets) kept in $DATA_DIR — remove manually if wanted."
    exit 0
}
[ "${1:-}" = "--uninstall" ] && uninstall
[ $# -gt 0 ] && die "unknown argument: $1 (only --uninstall is supported)"

# --- Preflight -------------------------------------------------------------
command -v node >/dev/null 2>&1 || die "node not found on PATH — install Node.js >= 18 first"
command -v npm  >/dev/null 2>&1 || die "npm not found on PATH — install Node.js >= 18 first"
NODE_MAJOR=$(node -p 'process.versions.node.split(".")[0]')
[ "$NODE_MAJOR" -ge 18 ] 2>/dev/null || die "Node.js >= 18 required (found $(node --version))"
if ! command -v claude >/dev/null 2>&1; then
    printf 'warning: `claude` CLI not found on PATH. The server needs it installed AND authenticated\n' >&2
    printf '         (run `claude` once interactively) before sessions can run.\n' >&2
fi

# Locate payload: release tarball layout (flat next to this script) or repo layout.
if [ -f "$HERE/agentic-dev-server" ]; then
    BIN_SRC="$HERE/agentic-dev-server"; BRIDGE_SRC="$HERE"; TPL_DIR="$HERE"
elif [ -f "$HERE/../server-rs/target/release/agentic-dev-server" ]; then
    BIN_SRC="$HERE/../server-rs/target/release/agentic-dev-server"; BRIDGE_SRC="$HERE/../server-rs"; TPL_DIR="$HERE"
else
    die "agentic-dev-server binary not found next to install.sh (release tarball) or in ../server-rs/target/release (run \`make build\` first)"
fi

# --- Stage SDK bridge deps FIRST -------------------------------------------
# A failed npm run must leave a currently-running install completely untouched (the live service
# resolves node_modules next to the bridge per turn), so stage in a temp dir from the PAYLOAD's
# package files and only touch $INSTALL_DIR after npm succeeds.
info "installing SDK bridge dependencies (npm)"
NPM_STAGE=$(mktemp -d)
trap 'rm -rf "$NPM_STAGE"' EXIT
cp "$BRIDGE_SRC/package.json" "$NPM_STAGE/"
if [ -f "$BRIDGE_SRC/package-lock.json" ]; then
    cp "$BRIDGE_SRC/package-lock.json" "$NPM_STAGE/"
    (cd "$NPM_STAGE" && npm ci --omit=dev --no-fund --no-audit --loglevel=error)
else
    (cd "$NPM_STAGE" && npm install --omit=dev --no-fund --no-audit --loglevel=error)
fi

# --- Install files (only after deps staged OK) ------------------------------
info "installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR" "$DATA_DIR"
install -m 755 "$BIN_SRC" "$INSTALL_DIR/agentic-dev-server"
install -m 644 "$BRIDGE_SRC/sdk-bridge.mjs" "$INSTALL_DIR/sdk-bridge.mjs"
install -m 644 "$BRIDGE_SRC/package.json" "$INSTALL_DIR/package.json"
[ -f "$BRIDGE_SRC/package-lock.json" ] && install -m 644 "$BRIDGE_SRC/package-lock.json" "$INSTALL_DIR/package-lock.json"
rm -rf "$INSTALL_DIR/node_modules"
mv "$NPM_STAGE/node_modules" "$INSTALL_DIR/node_modules"

# --- Service PATH: keep whatever node/claude the preflight actually found (nvm/asdf/brew) ---
NODE_DIR=$(dirname "$(command -v node)")
CLAUDE_DIR=""
command -v claude >/dev/null 2>&1 && CLAUDE_DIR=$(dirname "$(command -v claude)")
SVC_PATH="$HOME/.local/bin:$NODE_DIR${CLAUDE_DIR:+:$CLAUDE_DIR}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

# --- Secrets (generated once; never printed) --------------------------------
if [ ! -f "$ENV_FILE" ]; then
    info "generating secrets in $ENV_FILE"
    umask 077
    if command -v openssl >/dev/null 2>&1; then
        PW=$(openssl rand -hex 6); SECRET=$(openssl rand -hex 24)
    else
        PW=$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n'); SECRET=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
    fi
    printf 'AGENTIC_PASSWORD=%s\nAGENTIC_AUTH_SECRET=%s\n' "$PW" "$SECRET" > "$ENV_FILE"
    unset PW SECRET
fi

# --- Service ---------------------------------------------------------------
if [ "$OS" = "Darwin" ]; then
    PLIST_SRC="$TPL_DIR/agentic-dev.launchd.plist"
    [ -f "$PLIST_SRC" ] || die "agentic-dev.launchd.plist not found next to install.sh"
    PLIST_DST="$HOME/Library/LaunchAgents/dev.agentic.server.plist"
    mkdir -p "$HOME/Library/LaunchAgents"
    sed -e "s|@HOME@|$HOME|g" -e "s|@INSTALL_DIR@|$INSTALL_DIR|g" -e "s|@SVC_PATH@|$SVC_PATH|g" \
        "$PLIST_SRC" > "$PLIST_DST"
    launchctl bootout "gui/$(id -u)/dev.agentic.server" 2>/dev/null || true
    # bootout teardown is asynchronous — retry bootstrap briefly on an upgrade.
    tries=0
    until launchctl bootstrap "gui/$(id -u)" "$PLIST_DST" 2>/dev/null; do
        tries=$((tries + 1))
        [ "$tries" -ge 10 ] && die "launchctl bootstrap failed after ${tries}s (try: launchctl bootstrap gui/\$(id -u) $PLIST_DST)"
        sleep 1
    done
    launchctl kickstart -k "gui/$(id -u)/dev.agentic.server"
    info "launchd agent installed (dev.agentic.server); logs: $DATA_DIR/server.log"
elif command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    UNIT_SRC="$TPL_DIR/agentic-dev.service"
    [ -f "$UNIT_SRC" ] || die "agentic-dev.service not found next to install.sh"
    mkdir -p "$HOME/.config/systemd/user"
    # Bake in the real install dir (AGENTIC_INSTALL_DIR support) and the PATH that passed preflight.
    sed -e "s|^ExecStart=.*|ExecStart=$INSTALL_DIR/agentic-dev-server|" \
        -e "s|^Environment=PATH=.*|Environment=PATH=$SVC_PATH|" \
        "$UNIT_SRC" > "$HOME/.config/systemd/user/agentic-dev.service"
    systemctl --user daemon-reload
    systemctl --user enable --now agentic-dev
    systemctl --user restart agentic-dev
    info "systemd user service installed; logs: journalctl --user -u agentic-dev -f"
    info "to survive reboot without login: sudo loginctl enable-linger $USER"
else
    info "no systemd/launchd detected — run manually:"
    info "  set -a; . $ENV_FILE; set +a; $INSTALL_DIR/agentic-dev-server"
fi

PORT=7420
info "done. API: http://<this-host>:$PORT — point the agentic-dev Android app at it."
info "login password: cat $ENV_FILE   (never share the AUTH_SECRET)"
info "requires the \`claude\` CLI authenticated for the user running the service."

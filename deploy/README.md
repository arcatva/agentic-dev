# Install agentic-dev as a persistent service

Runs `:7420` as a background service — `systemd --user` on Linux, a launchd user agent on macOS —
with auto-restart on crash.

> **Restarting kills in-flight turns.** The platform is streaming-only: each session is a persistent
> child process whose stdin pipe dies with the service, so a restart terminates running turns (the next
> boot finalizes them as `interrupted` and the user resumes). Prefer restarting when sessions are idle.

## Prerequisites (both platforms)

- Node.js ≥ 18 with npm (runs the per-turn SDK bridge)
- the `claude` CLI installed **and authenticated** for the user running the service
  (sessions draw from the monthly Agent SDK credit)
- an Anthropic credential for title generation: `ANTHROPIC_AUTH_TOKEN` env or
  `~/.claude/.credentials.json` (created by `claude` login) — the server refuses to boot without one

## Install

**From a release tarball** (GitHub Releases → pick your platform):

```bash
tar xzf agentic-dev-<version>-<platform>.tar.gz
cd agentic-dev-<version>-<platform>
./install.sh
```

**From source** (same installer, repo layout is auto-detected):

```bash
cd ~/src/agentic-dev && make build && ./deploy/install.sh
```

The installer:
1. copies `agentic-dev-server` + `sdk-bridge.mjs` to `~/.local/share/agentic-dev/`
2. runs `npm ci` there for the bridge's Claude Agent SDK deps
3. generates secrets into `~/.agentic-dev/service.env` (mode 600, only on first install) —
   recover the login password later with `cat ~/.agentic-dev/service.env`
4. registers + starts the service (`agentic-dev.service` uses systemd `%h`, so it works for any
   user; the launchd plist gets `@HOME@` substituted at install time)

After install (Linux):

```bash
sudo loginctl enable-linger "$USER"             # survive reboot without an active login
sudo ufw allow 7420/tcp comment 'agentic-dev'   # open the firewall (LAN/tailscale)
```

Uninstall: `./install.sh --uninstall` (keeps `~/.agentic-dev/` data; remove manually if wanted).

## HTTPS

**HTTPS is on by default** — the server terminates TLS itself, no reverse proxy required. On first
boot it generates a self-signed cert under `~/.agentic-dev/tls/` (SANs = localhost + loopback +
every local interface IP, incl. LAN/tailscale). Nothing to configure: point the Android app at
`https://<host-or-ip>:7420` and confirm the cert fingerprint on first connect (trust-on-first-use).
The fingerprint is printed in the boot log and downloadable at `GET /api/tls/cert.pem`.

Config in `~/.agentic-dev/service.env` (restart the service to apply):

**Bring your own cert** (a real CA / certbot cert — then the app trusts it with no pinning prompt):

```ini
AGENTIC_TLS_CERT=/etc/letsencrypt/live/agentic.example.com/fullchain.pem
AGENTIC_TLS_KEY=/etc/letsencrypt/live/agentic.example.com/privkey.pem
```

**Extra SANs** for an IP/hostname the auto-detector misses (e.g. a public/NAT address):

```ini
AGENTIC_TLS_SAN=203.0.113.9,agentic.example.com
```

**Plain HTTP** (e.g. you front it with your own TLS proxy): `AGENTIC_TLS=off`.

If you serve on port 443 directly, open it and let the non-root service bind low ports:

```bash
sudo ufw allow 443/tcp comment 'agentic-dev https'
sudo setcap 'cap_net_bind_service=+ep' ~/.local/share/agentic-dev/agentic-dev-server
```

> **`setcap` note:** it is cleared whenever `install.sh` overwrites the binary on upgrade, so
> re-run it after each upgrade — or run the service as root. (The default port 7420 needs no setcap.)

## Manage

Linux:

```bash
systemctl --user status agentic-dev
systemctl --user restart agentic-dev      # apply a new build
journalctl --user -u agentic-dev -f       # logs
```

macOS:

```bash
launchctl print "gui/$(id -u)/dev.agentic.server"           # status
launchctl kickstart -k "gui/$(id -u)/dev.agentic.server"    # restart
tail -f ~/.agentic-dev/server.log                            # logs
```

## Upgrade

Unpack the new tarball and run `./install.sh` again (or `make build && ./deploy/install.sh` from
source) — it overwrites the binary/bridge, keeps your secrets, and restarts the service.

Paths: sqlite + per-session logs in `~/.agentic-dev/`; self-signed TLS cert in `~/.agentic-dev/tls/`;
session worktrees in `~/src/agentic-worktrees/`.
The API listens at `https://<host-ip>:7420` (LAN) or the host's tailscale IP — the agentic-dev Android
app connects here (`AGENTIC_TLS=off` for plain HTTP).

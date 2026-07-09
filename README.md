# agentic-dev

Agent-driven local dev platform. Submit a request against a `~/src` repo; it creates a git
worktree and runs a headless autonomous `claude` session in it, streaming output over an HTTP +
WebSocket API. The client is the **agentic-dev Android app** (`~/src/agentic-dev-android`); this
repo is the backend/API only — the old in-repo web UI has been removed.

## Install (release tarball — Linux & macOS)
Grab the tarball for your platform from the GitHub Releases page, then:

    tar xzf agentic-dev-<version>-<platform>.tar.gz
    cd agentic-dev-<version>-<platform>
    ./install.sh

The installer copies the server + SDK bridge to `~/.local/share/agentic-dev/`, installs the
bridge's npm deps, generates login secrets in `~/.agentic-dev/service.env`, and registers a
service (systemd `--user` on Linux, launchd agent on macOS). Prerequisites: Node.js ≥ 18 with
npm, and the `claude` CLI installed **and authenticated** for the service user.
Uninstall with `./install.sh --uninstall`.

## Run from source
    AGENTIC_PASSWORD=<pick-one> make run     # installs the bridge's Claude SDK + builds, then runs
The API listens on https://<this-host>:7420 (LAN/tailscale) with an auto self-signed cert — see
[HTTPS](#https). Point the Android app's Host at it (`AGENTIC_TLS=off` for plain HTTP).

## Config (env)
| Var | Default | Meaning |
|---|---|---|
| AGENTIC_PASSWORD | (required) | login password |
| AGENTIC_PORT | 7420 | listen port |
| AGENTIC_HOST | 0.0.0.0 | bind address (set to tailscale IP to restrict) |
| AGENTIC_SRC_ROOT | ~/src | repos root |
| AGENTIC_MAX_CONCURRENT | (unset = unlimited) | optional cap on concurrent sessions |
| AGENTIC_NODE_BIN | node | node binary that runs the SDK bridge |
| AGENTIC_SDK_BRIDGE | (bundled) | path to sdk-bridge.mjs override |
| AGENTIC_TLS | on | `off`/`0`/`false` → serve plain HTTP instead of HTTPS |
| AGENTIC_TLS_CERT | (unset) | bring-your-own PEM cert chain (needs `AGENTIC_TLS_KEY`) |
| AGENTIC_TLS_KEY | (unset) | bring-your-own PEM private key |
| AGENTIC_TLS_DIR | ~/.agentic-dev/tls | where the auto self-signed cert+key live |
| AGENTIC_TLS_SAN | (unset) | extra cert SANs (IP or DNS), comma-separated |
| AGENTIC_TLS_REGEN | (off) | `1`/`true` → force-regenerate the self-signed cert on boot |

Requires the `claude` CLI on PATH and authenticated. Sessions draw from the monthly Agent
SDK credit — watch the cost total.

## HTTPS
**HTTPS is on by default.** On first boot the server generates a **self-signed certificate**
(persisted under `AGENTIC_TLS_DIR`) whose SANs include `localhost`, the loopback addresses, and
every local interface IP (LAN, tailscale, …) — plus anything in `AGENTIC_TLS_SAN`. So it validates
when you connect by IP, e.g. `https://192.168.1.10:7420`, no domain required.

- **Trusting it (Android):** the app uses **trust-on-first-use** — on the first connection it shows
  the cert's SHA-256 fingerprint (also printed in the server log at boot, and downloadable at
  `GET /api/tls/cert.pem`); confirm it once and the app pins that cert. Because it pins the cert
  identity (not a hostname), it keeps working for any IP you enter and across IP changes.
- **Bring your own cert** (a real CA / certbot cert, so no pinning is needed):

      AGENTIC_TLS_CERT=/etc/letsencrypt/live/host/fullchain.pem
      AGENTIC_TLS_KEY=/etc/letsencrypt/live/host/privkey.pem

- **Plain HTTP** (e.g. behind your own TLS-terminating proxy): `AGENTIC_TLS=off`.

Set the Android app's Host to `https://<host-or-ip>` — the client derives `wss://` automatically.
The self-signed cert+key are regular PEM files under `AGENTIC_TLS_DIR`; replace them (or point
`AGENTIC_TLS_CERT`/`KEY` elsewhere) to use your own, or set `AGENTIC_TLS_REGEN=1` to rotate.

## Test
    make test     # never hits real claude (no API cost)

## License
MIT — see [LICENSE](LICENSE).

# GhostView

A dual-release HTML5 remote assistance platform with two parallel products sharing one signaling and transport backbone:

- **GhostView Lite** — Pure browser-to-browser screen sharing via `getDisplayMedia()`. Zero install, any modern browser.
- **GhostView Pro** — A portable Tauri 2.0 native agent for Windows with higher-quality GPU-accelerated capture via `windows-capture`. Ready for Phase 2 remote control.

Both share the same WebRTC transport, 6-digit PIN session model, and viewer UI. The only difference is the host-side capture mechanism.

## Status

Phase 1 — view-only remote assistance. See the in-repo design doc (commit message and `CLAUDE.md` history) for the full roadmap.

| | Lite (browser host) | Pro (Tauri host) |
| --- | --- | --- |
| Host install | None | Portable `.exe`, no admin |
| Capture | `getDisplayMedia()` | `windows-capture` (Windows-only) |
| Frame rate | 15–30 FPS | Up to 60 FPS |
| Remote control (Phase 2) | Never | Planned via `enigo` |
| Status | Fully implemented | Wired — capture → VP9 → WebRTC → signaling roundtrip passing |

## Repository layout

```
ghostview/
├── server/              # Node.js signaling server + shared HTML viewer + Lite host
│   ├── server.js
│   ├── session-manager.js
│   ├── public/
│   │   ├── viewer.html
│   │   ├── host-lite.html
│   │   └── assets/
│   └── Dockerfile
├── tauri-host/          # GhostView Pro — Tauri 2.0 scaffold
│   ├── src-tauri/       # Rust backend (capture, WebRTC, signaling)
│   └── src/             # Minimal HTML/CSS/JS frontend
├── docker-compose.yml       # Signaling + coturn (STUN/TURN)
├── turnserver.conf.example  # coturn config TEMPLATE — copy to turnserver.conf (gitignored) and edit
└── certs/                   # TLS certs (not committed)
```

## Quick start — local dev

### 1. Signaling server + Lite

```bash
cd server
npm install

# Generate a self-signed cert (getDisplayMedia requires HTTPS).
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -keyout certs/key.pem \
  -out certs/cert.pem -days 365 -nodes -subj '/CN=localhost'

node server.js
# → listening on https://0.0.0.0:8443
```

Open two browser tabs:

- Host: <https://localhost:8443/host-lite.html> — click "Start Sharing", note the 6-digit PIN.
- Viewer: <https://localhost:8443/> — enter the PIN, click Connect.

Your browser will warn about the self-signed cert; accept it in both tabs.

### 2. TURN Server (coturn) Setup

GhostView uses coturn for STUN/TURN relay (required for NAT traversal).

#### Development (Self-Signed)

```bash
cp turnserver.conf.example turnserver.conf
# Edit turnserver.conf: set realm, username, password
docker compose up -d coturn
```

#### Production (Environment-Injected Secrets)

Do NOT commit `turnserver.conf` with real credentials. Instead:

1. Create secrets via environment variables:
   ```bash
   export TURN_REALM=your-domain.com
   export TURN_USER=bot-user
   export TURN_PASSWORD=$(openssl rand -base64 32)
   export TURN_SECRET=$(openssl rand -base64 32)
   ```

2. Inject at container startup via entrypoint script:
   ```bash
   # docker-compose.yml coturn service:
   environment:
     TURN_REALM: ${TURN_REALM}
     TURN_USER: ${TURN_USER}
     TURN_PASSWORD: ${TURN_PASSWORD}
   # Entrypoint script generates turnserver.conf from template
   ```

3. Use short-term credentials for production:
   - Enable `use-auth-secret` mode in coturn
   - Signaling server rotates HMAC-SHA1 credentials every 10 minutes (Phase 2)

**Important:** `turnserver.conf` is gitignored so credentials never reach the repo. Verify with `git grep -n "supersecretpassword"` that no test credentials accidentally leak.

#### TURNS (TLS-wrapped TURN) for corporate firewalls

If your network blocks plain UDP 3478, enable TURNS in `turnserver.conf`:

```
cert=/path/to/cert.pem
pkey=/path/to/key.pem
cipher-list=ECDHE+AESGCM:ECDHE+CHACHA20:DHE+AESGCM:DHE+CHACHA20
tls-listening-port=5349
```

Then reference the TURNS server in your ICE config (viewer, Lite host, Pro host) using `turns://<your-server>:5349`.

---

Reference the TURN server from the ICE config in `server/public/assets/ghostview-viewer.js` and `ghostview-host-lite.js`, and from the Tauri host via the `GHOSTVIEW_ICE_SERVERS` env var.

### 3. GhostView Pro scaffold

```bash
cd tauri-host/src-tauri
cargo check
```

The real WebRTC stack and VP9 encoder are always compiled in — there are no feature flags. Prerequisites:

```bash
# libvpx dev headers (required on all platforms):
sudo apt install libvpx-dev       # Debian/Ubuntu
brew install libvpx               # macOS
vcpkg install libvpx:x64-windows  # Windows (set VPX_LIB_DIR/VPX_VERSION if needed)

# Full dev build — Windows only (windows-capture is Windows-gated):
cargo tauri dev

# Run tests:
cargo test

# Optional: round-trip the Rust signaling client against the real Node server.
GHOSTVIEW_NODE_IT=1 cargo test --test signaling_roundtrip
```

Pro captures the screen via `windows-capture`; macOS/Linux `start_session` returns `UnsupportedPlatform` on Phase 1 by design.

Before producing a release bundle you must add `icons/icon.ico`, `icon.png`, `32x32.png`, `128x128.png`, and `128x128@2x.png`. See `tauri-host/src-tauri/icons/README.md`.

## Production deployment

```bash
# Put real TLS certs in ./certs/cert.pem and ./certs/key.pem.
cp turnserver.conf.example turnserver.conf
# Edit turnserver.conf: set realm, strong user password, and external-ip.
NODE_ENV=production \
ALLOWED_ORIGINS=https://your.domain \
docker compose up -d
```

`NODE_ENV=production` makes the signaling server refuse to start without TLS
certs (unless `ALLOW_INSECURE=1` is set — use only when a reverse proxy
terminates TLS) and without `ALLOWED_ORIGINS` set. See the Security model
section below.

One small VPS (1 vCPU, 512 MB RAM) handles ~100 concurrent sessions for signaling; TURN bandwidth is the real cost driver.

## Phase 2 roadmap

- **Remote control (keyboard + mouse)** via the `enigo` crate on the Pro host.
- **Clipboard sync** via `arboard`.
- **Short-term TURN credentials** — signaling server issues HMAC-SHA1
  time-limited username/password per-session, rotating a shared secret with
  coturn's `use-auth-secret` mode.
- **File transfer** through a WebRTC data channel.
- **macOS / Linux Pro host** — port the `windows-capture` code path to
  `screencapturekit` (macOS) and `pipewire` (Linux).

## Security model

- **Transport:** TLS for signaling, DTLS-SRTP for media (mandatory in WebRTC, cannot be disabled).
- **Session identity:** 6-digit PIN, cryptographically random (`crypto.randomInt`), single-use, 10-minute TTL.
- **Rate limiting:** 10 session-create attempts per IP per minute.
- **Origin check:** Set `ALLOWED_ORIGINS` env on the signaling server for production.
- **No persistence:** All session state lives in server memory; nothing is written to disk.
- **Consent model:** Hosts must explicitly start sharing. Viewers cannot initiate capture.
- **Pro host CSP:** Tauri's `tauri.conf.json` enforces a strict Content Security Policy (CSP) with no eval, no remote scripts, and limited inline styles. `dangerousRemoteDomainIpcAccess` is always empty in the committed config.

## License

TBD.

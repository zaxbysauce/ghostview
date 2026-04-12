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
| Status | Fully implemented | Scaffold — real capture/webrtc wiring pending |

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
├── docker-compose.yml   # Signaling + coturn (STUN/TURN)
├── turnserver.conf      # coturn config — edit before production use
└── certs/               # TLS certs (not committed)
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

### 2. STUN/TURN (optional, for connections across NAT)

```bash
# Edit turnserver.conf first — change the `user=` password and set `external-ip`.
docker compose up -d coturn
```

Then add your TURN server to the ICE config in `server/public/assets/ghostview-viewer.js` and `ghostview-host-lite.js`.

### 3. GhostView Pro scaffold

```bash
cd tauri-host/src-tauri
cargo check
```

The scaffold compiles cross-platform. Real capture and WebRTC are feature-gated:

```bash
# Enable real WebRTC stack (adds significant compile time + transitive deps):
cargo check --features real-webrtc

# Enable VP9 software encoder:
cargo check --features vpx

# Full dev build (Windows only — real native capture):
cargo tauri dev
```

Before producing a release bundle you must add `icons/icon.ico`, `icon.png`, `32x32.png`, `128x128.png`, and `128x128@2x.png`. See `tauri-host/src-tauri/icons/README.md`.

## Production deployment

```bash
# Put real TLS certs in ./certs/cert.pem and ./certs/key.pem.
# Edit turnserver.conf: set realm, user password, and external-ip.
docker compose up -d
```

One small VPS (1 vCPU, 512 MB RAM) handles ~100 concurrent sessions for signaling; TURN bandwidth is the real cost driver.

## Security model

- **Transport:** TLS for signaling, DTLS-SRTP for media (mandatory in WebRTC, cannot be disabled).
- **Session identity:** 6-digit PIN, cryptographically random (`crypto.randomInt`), single-use, 10-minute TTL.
- **Rate limiting:** 10 session-create attempts per IP per minute.
- **Origin check:** Set `ALLOWED_ORIGINS` env on the signaling server for production.
- **No persistence:** All session state lives in server memory; nothing is written to disk.
- **Consent model:** Hosts must explicitly start sharing. Viewers cannot initiate capture.

## License

TBD.

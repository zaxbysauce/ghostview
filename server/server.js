import { createServer as createHttpsServer } from 'https';
import { createServer as createHttpServer } from 'http';
import { readFileSync, existsSync } from 'fs';
import { WebSocketServer } from 'ws';
import { join, extname, normalize, resolve, sep } from 'path';
import SessionManager from './session-manager.js';

const PORT = Number(process.env.PORT || 8443);
const HOST = process.env.HOST || '0.0.0.0';
const ALLOWED_ORIGINS = (process.env.ALLOWED_ORIGINS || '').split(',').filter(Boolean);
// Only honor X-Forwarded-For when explicitly told we're behind a trusted proxy.
const TRUST_PROXY = /^(1|true|yes)$/i.test(process.env.TRUST_PROXY || '');
const IS_PRODUCTION = process.env.NODE_ENV === 'production';
const ALLOW_INSECURE = /^(1|true|yes)$/i.test(process.env.ALLOW_INSECURE || '');

// In production, an empty ALLOWED_ORIGINS would silently allow any origin —
// refuse to start. Dev keeps the permissive fallback so self-signed setups
// don't require the operator to set the env var.
if (IS_PRODUCTION && ALLOWED_ORIGINS.length === 0) {
  console.error(
    '[ghostview] FATAL: ALLOWED_ORIGINS must be set in production (comma-separated list of https:// origins)'
  );
  process.exit(1);
}

const sessions = new SessionManager();

const PUBLIC_DIR = resolve('./public');
const CERT_DIR = resolve('./certs');
const CERT_PATH = join(CERT_DIR, 'cert.pem');
const KEY_PATH = join(CERT_DIR, 'key.pem');

const useTLS = existsSync(CERT_PATH) && existsSync(KEY_PATH);

// In production, refuse to start without TLS unless the operator has
// explicitly opted in with ALLOW_INSECURE=1 (useful for TLS-terminating
// reverse proxies that forward plain HTTP to the app).
if (IS_PRODUCTION && !useTLS && !ALLOW_INSECURE) {
  console.error(
    '[ghostview] FATAL: TLS certs required in production. Place cert.pem + key.pem under ./certs, ' +
    'or set ALLOW_INSECURE=1 if terminating TLS at a reverse proxy.'
  );
  process.exit(1);
}

const httpServer = useTLS
  ? createHttpsServer({
      cert: readFileSync(CERT_PATH),
      key: readFileSync(KEY_PATH),
    })
  : createHttpServer();

if (!useTLS) {
  console.warn('[ghostview] ====================================================================');
  console.warn('[ghostview]  WARNING: TLS certs not found in ./certs — serving plain HTTP.');
  console.warn('[ghostview]  getDisplayMedia() requires HTTPS; browsers will refuse to capture');
  console.warn('[ghostview]  unless served over HTTPS or from localhost. Generate dev certs with:');
  console.warn('[ghostview]    openssl req -x509 -newkey rsa:2048 -keyout certs/key.pem \\');
  console.warn('[ghostview]      -out certs/cert.pem -days 365 -nodes -subj "/CN=localhost"');
  console.warn('[ghostview] ====================================================================');
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
};

httpServer.on('request', (req, res) => {
  if (req.url === '/health') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    return res.end(JSON.stringify({
      status: 'ok',
      sessions: sessions.sessions.size,
      tls: useTLS,
    }));
  }

  // Strip query + URL-decode
  let urlPath;
  try {
    urlPath = decodeURIComponent(req.url.split('?')[0]);
  } catch {
    res.writeHead(400);
    return res.end('Bad Request');
  }

  if (urlPath === '/') urlPath = '/viewer.html';
  if (urlPath === '/host' || urlPath === '/host-lite') urlPath = '/host-lite.html';

  // Resolve under PUBLIC_DIR and prevent traversal.
  // Must compare against PUBLIC_DIR + sep to reject sibling-prefix paths
  // (e.g. `/public-backup/...` must not be served when PUBLIC_DIR=`/.../public`).
  const requested = normalize(join(PUBLIC_DIR, urlPath));
  if (requested !== PUBLIC_DIR && !requested.startsWith(PUBLIC_DIR + sep)) {
    res.writeHead(403);
    return res.end('Forbidden');
  }

  try {
    const content = readFileSync(requested);
    const ext = extname(requested);
    const isHtml = ext === '.html';
    res.writeHead(200, {
      'Content-Type': MIME[ext] || 'application/octet-stream',
      'X-Content-Type-Options': 'nosniff',
      'X-Frame-Options': 'DENY',
      'Referrer-Policy': 'no-referrer',
      ...(useTLS ? { 'Strict-Transport-Security': 'max-age=31536000; includeSubDomains' } : {}),
      ...(isHtml ? {
        'Content-Security-Policy':
          "default-src 'self'; " +
          "connect-src 'self' ws: wss:; " +
          "script-src 'self'; " +
          "style-src 'self'; " +
          "img-src 'self' data:; " +
          "media-src 'self' blob:; " +
          "object-src 'none'; " +
          "base-uri 'self'; " +
          "frame-ancestors 'none'",
      } : {}),
      'Cache-Control': 'no-store',
    });
    res.end(content);
  } catch {
    res.writeHead(404);
    res.end('Not Found');
  }
});

// WebSocket signaling with origin check
const wss = new WebSocketServer({
  server: httpServer,
  verifyClient: (info, cb) => {
    if (ALLOWED_ORIGINS.length === 0) return cb(true); // permissive dev
    const origin = info.origin || info.req.headers.origin;
    if (ALLOWED_ORIGINS.includes(origin)) return cb(true);
    cb(false, 403, 'Origin not allowed');
  },
});

// Per-IP rate limiting. Separate buckets so aggressive create-session limits
// don't accidentally allow rampant join-session brute-forcing, and vice versa.
const RATE_WINDOW = 60_000;
const LIMITS = {
  create: 10,   // create-session per IP per minute
  join: 20,     // join-session attempts per IP per minute (PIN is 6 digits;
                // combined with per-connection wrong-PIN lockout below this
                // blocks brute-force enumeration)
};
const rateBuckets = {
  create: new Map(),
  join: new Map(),
};

function checkRateLimit(kind, ip) {
  const bucket = rateBuckets[kind];
  const limit = LIMITS[kind];
  const now = Date.now();
  const record = bucket.get(ip) || { count: 0, start: now };
  if (now - record.start > RATE_WINDOW) {
    record.count = 0;
    record.start = now;
  }
  record.count++;
  bucket.set(ip, record);
  return record.count <= limit;
}

// Periodic cleanup of rate-limit maps.
setInterval(() => {
  const now = Date.now();
  for (const bucket of Object.values(rateBuckets)) {
    for (const [ip, rec] of bucket) {
      if (now - rec.start > RATE_WINDOW * 2) bucket.delete(ip);
    }
  }
}, 5 * 60_000);

// Per-connection wrong-PIN counter: three misses and the socket is closed,
// preventing a viewer from enumerating PINs over a single persistent WS.
const MAX_WRONG_PINS = 3;

wss.on('connection', (ws, req) => {
  // Only honor X-Forwarded-For if operator opted in via TRUST_PROXY — otherwise
  // any client could spoof it to shard the rate-limit counter.
  const xff = TRUST_PROXY ? req.headers['x-forwarded-for'] : null;
  const ip = (xff?.split(',')[0].trim()) || req.socket.remoteAddress;

  // Per-connection counter for join-session misses.
  ws._wrongPins = 0;

  // Keepalive: ping every 25s. Most NATs drop idle UDP at 30s and idle TCP
  // well under 300s, so a 25s tempo keeps the path warm for the signaling
  // channel even when the WebRTC media path goes quiet. `ws.ping()` waits
  // for a pong frame; if none arrives before the next tick, the library's
  // internal tracking marks the socket as stale and subsequent `send()`s
  // will fail — our message handler already tolerates that.
  ws._isAlive = true;
  ws.on('pong', () => { ws._isAlive = true; });

  ws.on('message', (raw) => {
    let msg;
    try { msg = JSON.parse(raw); } catch { return; }
    if (!msg || typeof msg.type !== 'string') return;

    switch (msg.type) {
      case 'create-session': {
        if (!checkRateLimit('create', ip)) {
          ws.send(JSON.stringify({ type: 'error', error: 'rate_limited' }));
          return;
        }
        try {
          const pin = sessions.createSession(ws);
          ws.send(JSON.stringify({ type: 'session-created', pin }));
        } catch (err) {
          ws.send(JSON.stringify({ type: 'error', error: 'server_error' }));
        }
        break;
      }
      case 'join-session': {
        if (!checkRateLimit('join', ip)) {
          ws.send(JSON.stringify({ type: 'error', error: 'rate_limited' }));
          return;
        }
        if (typeof msg.pin !== 'string' || !/^\d{6}$/.test(msg.pin)) {
          ws.send(JSON.stringify({ type: 'error', error: 'invalid_pin' }));
          return;
        }
        const result = sessions.joinSession(msg.pin, ws);
        if (result.error) {
          ws._wrongPins += 1;
          ws.send(JSON.stringify({ type: 'error', error: result.error }));
          if (ws._wrongPins >= MAX_WRONG_PINS) {
            try { ws.close(1008, 'too_many_wrong_pins'); } catch {}
          }
        } else {
          ws.send(JSON.stringify({ type: 'session-joined' }));
          if (result.session.host?.readyState === 1) {
            result.session.host.send(JSON.stringify({ type: 'viewer-joined' }));
          }
        }
        break;
      }
      case 'offer':
      case 'answer':
      case 'ice-candidate':
        sessions.relay(ws, msg);
        break;
      case 'end-session':
        sessions.handleDisconnect(ws);
        break;
    }
  });

  ws.on('close', () => sessions.handleDisconnect(ws));
  ws.on('error', () => sessions.handleDisconnect(ws));
});

// Keepalive sweep: ping every 25s; terminate any socket that didn't pong
// since the previous sweep. Bounded overhead — O(clients).
const KEEPALIVE_INTERVAL = 25_000;
const keepaliveTimer = setInterval(() => {
  for (const ws of wss.clients) {
    if (ws._isAlive === false) {
      try { ws.terminate(); } catch {}
      continue;
    }
    ws._isAlive = false;
    try { ws.ping(); } catch {}
  }
}, KEEPALIVE_INTERVAL);
keepaliveTimer.unref?.();

httpServer.listen(PORT, HOST, () => {
  const scheme = useTLS ? 'https' : 'http';
  console.log(`[ghostview] signaling server listening on ${scheme}://${HOST}:${PORT}`);
});

// Graceful shutdown — notify active peers, close the WS server, then HTTP.
let shuttingDown = false;
function shutdown(signal) {
  if (shuttingDown) return;
  shuttingDown = true;
  console.log(`[ghostview] received ${signal}, shutting down`);

  for (const ws of wss.clients) {
    try { ws.send(JSON.stringify({ type: 'error', error: 'server_shutdown' })); } catch {}
    try { ws.close(1001, 'server_shutdown'); } catch {}
  }

  wss.close(() => {
    httpServer.close(() => process.exit(0));
  });

  // Hard exit if cleanup stalls.
  setTimeout(() => process.exit(1), 5_000).unref();
}

process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('SIGINT', () => shutdown('SIGINT'));

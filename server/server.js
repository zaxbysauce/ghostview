import { createServer as createHttpsServer } from 'https';
import { createServer as createHttpServer } from 'http';
import { readFileSync, existsSync } from 'fs';
import { WebSocketServer } from 'ws';
import { join, extname, normalize, resolve } from 'path';
import SessionManager from './session-manager.js';

const PORT = Number(process.env.PORT || 8443);
const HOST = process.env.HOST || '0.0.0.0';
const ALLOWED_ORIGINS = (process.env.ALLOWED_ORIGINS || '').split(',').filter(Boolean);

const sessions = new SessionManager();

const PUBLIC_DIR = resolve('./public');
const CERT_DIR = resolve('./certs');
const CERT_PATH = join(CERT_DIR, 'cert.pem');
const KEY_PATH = join(CERT_DIR, 'key.pem');

const useTLS = existsSync(CERT_PATH) && existsSync(KEY_PATH);
const httpServer = useTLS
  ? createHttpsServer({
      cert: readFileSync(CERT_PATH),
      key: readFileSync(KEY_PATH),
    })
  : createHttpServer();

if (!useTLS) {
  console.warn('[ghostview] TLS certs not found in ./certs — serving plain HTTP.');
  console.warn('[ghostview] getDisplayMedia requires HTTPS; generate self-signed certs for dev.');
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

  // Resolve under PUBLIC_DIR and prevent traversal
  const requested = normalize(join(PUBLIC_DIR, urlPath));
  if (!requested.startsWith(PUBLIC_DIR)) {
    res.writeHead(403);
    return res.end('Forbidden');
  }

  try {
    const content = readFileSync(requested);
    const ext = extname(requested);
    res.writeHead(200, {
      'Content-Type': MIME[ext] || 'application/octet-stream',
      'X-Content-Type-Options': 'nosniff',
      'X-Frame-Options': 'DENY',
      'Referrer-Policy': 'no-referrer',
      ...(useTLS ? { 'Strict-Transport-Security': 'max-age=31536000; includeSubDomains' } : {}),
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

// Per-IP rate limiting
const ipAttempts = new Map();
const RATE_LIMIT = 10;
const RATE_WINDOW = 60_000;

function checkRateLimit(ip) {
  const now = Date.now();
  const record = ipAttempts.get(ip) || { count: 0, start: now };
  if (now - record.start > RATE_WINDOW) {
    record.count = 0;
    record.start = now;
  }
  record.count++;
  ipAttempts.set(ip, record);
  return record.count <= RATE_LIMIT;
}

// Periodic cleanup of rate-limit map
setInterval(() => {
  const now = Date.now();
  for (const [ip, rec] of ipAttempts) {
    if (now - rec.start > RATE_WINDOW * 2) ipAttempts.delete(ip);
  }
}, 5 * 60_000);

wss.on('connection', (ws, req) => {
  const ip = (req.headers['x-forwarded-for']?.split(',')[0].trim())
    || req.socket.remoteAddress;

  ws.on('message', (raw) => {
    let msg;
    try { msg = JSON.parse(raw); } catch { return; }

    switch (msg.type) {
      case 'create-session': {
        if (!checkRateLimit(ip)) {
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
        if (typeof msg.pin !== 'string' || !/^\d{6}$/.test(msg.pin)) {
          ws.send(JSON.stringify({ type: 'error', error: 'invalid_pin' }));
          return;
        }
        const result = sessions.joinSession(msg.pin, ws);
        if (result.error) {
          ws.send(JSON.stringify({ type: 'error', error: result.error }));
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

httpServer.listen(PORT, HOST, () => {
  const scheme = useTLS ? 'https' : 'http';
  console.log(`[ghostview] signaling server listening on ${scheme}://${HOST}:${PORT}`);
});

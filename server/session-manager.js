import crypto from 'crypto';

class SessionManager {
  constructor() {
    this.sessions = new Map();        // PIN → { host, viewer, created, state }
    this.hostToPin = new Map();       // ws → PIN (reverse lookup)
    this.PIN_TTL = 10 * 60 * 1000;   // 10 minutes
    this.CLEANUP_INTERVAL = 60_000;

    setInterval(() => this._cleanup(), this.CLEANUP_INTERVAL);
  }

  generatePin() {
    let pin;
    let attempts = 0;
    // crypto.randomInt(min, max) has an EXCLUSIVE upper bound, so use 1_000_000
    // to cover the full 100000–999999 range. PINs are always 6 digits.
    do {
      pin = String(crypto.randomInt(100000, 1_000_000));
      attempts++;
    } while (this.sessions.has(pin) && attempts < 100);

    if (attempts >= 100) throw new Error('PIN space exhausted');
    return pin;
  }

  createSession(hostWs) {
    const pin = this.generatePin();
    this.sessions.set(pin, {
      host: hostWs,
      viewer: null,
      created: Date.now(),
      state: 'waiting'    // waiting → paired → active → closed
    });
    this.hostToPin.set(hostWs, pin);
    return pin;
  }

  joinSession(pin, viewerWs) {
    const session = this.sessions.get(pin);
    if (!session) return { error: 'invalid_pin' };
    if (session.state !== 'waiting') return { error: 'session_in_use' };
    if (Date.now() - session.created > this.PIN_TTL) {
      this.destroySession(pin);
      return { error: 'pin_expired' };
    }
    session.viewer = viewerWs;
    session.state = 'paired';
    return { success: true, session };
  }

  relay(senderWs, message) {
    const pin = this.hostToPin.get(senderWs)
      || this._findPinByViewer(senderWs);
    if (!pin) return;
    const session = this.sessions.get(pin);
    if (!session) return;

    const target = (senderWs === session.host) ? session.viewer : session.host;
    if (target?.readyState === 1) {
      target.send(JSON.stringify(message));
    }
  }

  destroySession(pin) {
    const session = this.sessions.get(pin);
    if (!session) return;
    this.hostToPin.delete(session.host);
    this.sessions.delete(pin);
  }

  handleDisconnect(ws) {
    const pin = this.hostToPin.get(ws) || this._findPinByViewer(ws);
    if (pin) {
      const session = this.sessions.get(pin);
      const other = (ws === session?.host) ? session?.viewer : session?.host;
      if (other?.readyState === 1) {
        other.send(JSON.stringify({ type: 'peer-disconnected' }));
      }
      this.destroySession(pin);
    }
  }

  _findPinByViewer(ws) {
    for (const [pin, session] of this.sessions) {
      if (session.viewer === ws) return pin;
    }
    return null;
  }

  _cleanup() {
    const now = Date.now();
    for (const [pin, session] of this.sessions) {
      if (session.state === 'waiting' && now - session.created > this.PIN_TTL) {
        if (session.host?.readyState === 1) {
          session.host.send(JSON.stringify({ type: 'session-expired' }));
        }
        this.destroySession(pin);
      }
    }
  }
}

export default SessionManager;

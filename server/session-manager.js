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
    if (session.state !== 'waiting') {
      // Log internal error state for debugging but return generic error to client
      console.warn('[ghostview] session in use for pin %s, state: %s', pin, session.state);
      return { error: 'invalid_pin' };
    }
    if (Date.now() - session.created > this.PIN_TTL) {
      // Log expiry internally but return generic error to client
      console.info('[ghostview] pin expired for pin %s, ttl: %d ms', pin, this.PIN_TTL);
      this.destroySession(pin);
      return { error: 'invalid_pin' };
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
      // ws.send throws synchronously on some transport failures; never let
      // a relay failure kill the calling message handler and leak the
      // session — log and continue. handleDisconnect will tidy up later.
      try {
        target.send(JSON.stringify(message));
      } catch (err) {
        console.warn('[ghostview] relay send failed:', err?.message || err);
      }
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
    if (this._cleanupRunning) return; // non-reentrant
    this._cleanupRunning = true;
    try {
      const now = Date.now();
      for (const [pin, session] of this.sessions) {
        if (session.state === 'waiting' && now - session.created > this.PIN_TTL) {
          // Wrap the whole iteration so a send failure cannot skip the
          // destroy — expired sessions MUST be reclaimed regardless.
          try {
            if (session.host?.readyState === 1) {
              session.host.send(JSON.stringify({ type: 'session-expired' }));
            }
          } catch (err) {
            console.warn('[ghostview] session-expired notify failed:', err?.message || err);
          }
          try {
            this.destroySession(pin);
          } catch (err) {
            console.error('[ghostview] destroySession failed:', err?.message || err);
          }
        }
      }
    } finally {
      this._cleanupRunning = false;
    }
  }
}

export default SessionManager;

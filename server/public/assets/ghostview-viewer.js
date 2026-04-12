(function () {
  'use strict';

  const DEFAULT_ICE_SERVERS = [
    { urls: 'stun:stun.l.google.com:19302' },
    { urls: 'stun:stun1.l.google.com:19302' },
    // {
    //   urls: 'turn:turn.example.com:3478',
    //   username: 'user',
    //   credential: 'pass'
    // },
  ];

  class GhostViewViewer {
    constructor(options = {}) {
      this.signalingUrl = options.signalingUrl;
      this.iceServers = options.iceServers || DEFAULT_ICE_SERVERS;
      this.videoElement = options.videoElement || null;
      this.onStatus = options.onStatus || (() => {});
      this.onError = options.onError || (() => {});
      this.onStream = options.onStream || (() => {});
      this.onClose = options.onClose || (() => {});

      this.ws = null;
      this.pc = null;
      this.pin = null;
      this.pendingCandidates = [];
      this.remoteDescSet = false;
      this.closed = false;
    }

    async connect(pin) {
      if (!/^\d{6}$/.test(pin)) {
        this.onError('invalid_pin');
        return;
      }
      this.pin = pin;
      this.closed = false;
      this._setStatus('connecting');

      try {
        await this._openSocket();
        this._send({ type: 'join-session', pin });
      } catch (err) {
        this.onError('failed');
        this._setStatus('error');
      }
    }

    _openSocket() {
      return new Promise((resolve, reject) => {
        const ws = new WebSocket(this.signalingUrl);
        this.ws = ws;
        let opened = false;

        ws.addEventListener('open', () => {
          opened = true;
          resolve();
        });

        ws.addEventListener('message', (ev) => this._onSignal(ev));

        ws.addEventListener('error', () => {
          if (!opened) reject(new Error('ws_open_failed'));
        });

        ws.addEventListener('close', () => {
          if (!this.closed) {
            this._setStatus('disconnected');
            this.onClose();
          }
        });
      });
    }

    _send(obj) {
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify(obj));
      }
    }

    async _onSignal(ev) {
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }

      switch (msg.type) {
        case 'session-joined':
          this._setStatus('waiting');
          this._createPeerConnection();
          break;

        case 'offer':
          await this._handleOffer(msg);
          break;

        case 'ice-candidate':
          await this._handleRemoteIce(msg);
          break;

        case 'peer-disconnected':
          this._setStatus('disconnected');
          this.onError('peer_disconnected');
          this.close();
          break;

        case 'session-expired':
          this.onError('pin_expired');
          this.close();
          break;

        case 'error':
          this.onError(msg.error || 'failed');
          this._setStatus('error');
          this.close();
          break;
      }
    }

    _createPeerConnection() {
      const pc = new RTCPeerConnection({ iceServers: this.iceServers });
      this.pc = pc;

      pc.addEventListener('icecandidate', (ev) => {
        if (ev.candidate) {
          this._send({ type: 'ice-candidate', candidate: ev.candidate });
        }
      });

      pc.addEventListener('track', (ev) => {
        const [stream] = ev.streams;
        if (this.videoElement && stream) {
          this.videoElement.srcObject = stream;
        }
        this.onStream(stream);
        this._setStatus('streaming');
      });

      pc.addEventListener('connectionstatechange', () => {
        const state = pc.connectionState;
        if (state === 'connected') {
          this._setStatus('streaming');
        } else if (state === 'disconnected') {
          this._setStatus('connecting');
        } else if (state === 'failed') {
          this._setStatus('error');
          this.onError('connection_failed');
        } else if (state === 'closed') {
          this._setStatus('disconnected');
        }
      });
    }

    async _handleOffer(msg) {
      if (!this.pc) this._createPeerConnection();
      try {
        await this.pc.setRemoteDescription(new RTCSessionDescription(msg.sdp || msg));
        this.remoteDescSet = true;
        for (const c of this.pendingCandidates) {
          try { await this.pc.addIceCandidate(c); } catch { /* ignore */ }
        }
        this.pendingCandidates = [];
        const answer = await this.pc.createAnswer();
        await this.pc.setLocalDescription(answer);
        this._send({ type: 'answer', sdp: answer });
      } catch (err) {
        this.onError('negotiation_failed');
      }
    }

    async _handleRemoteIce(msg) {
      const candidate = msg.candidate;
      if (!candidate) return;
      if (!this.pc || !this.remoteDescSet) {
        this.pendingCandidates.push(candidate);
        return;
      }
      try {
        await this.pc.addIceCandidate(candidate);
      } catch {
        /* ignore bad candidate */
      }
    }

    _setStatus(status) {
      this.status = status;
      this.onStatus(status);
    }

    close() {
      if (this.closed) return;
      this.closed = true;
      try { this._send({ type: 'end-session' }); } catch { /* ignore */ }
      if (this.pc) {
        try { this.pc.close(); } catch { /* ignore */ }
        this.pc = null;
      }
      if (this.ws) {
        try { this.ws.close(); } catch { /* ignore */ }
        this.ws = null;
      }
      if (this.videoElement) {
        try { this.videoElement.srcObject = null; } catch { /* ignore */ }
      }
      this._setStatus('idle');
    }
  }

  window.GhostViewViewer = GhostViewViewer;
})();

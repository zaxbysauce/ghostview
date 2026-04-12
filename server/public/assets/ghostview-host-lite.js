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

  class GhostViewHostLite {
    constructor(options = {}) {
      this.signalingUrl = options.signalingUrl;
      this.iceServers = options.iceServers || DEFAULT_ICE_SERVERS;
      this.onStatus = options.onStatus || (() => {});
      this.onPin = options.onPin || (() => {});
      this.onError = options.onError || (() => {});
      this.onEnded = options.onEnded || (() => {});
      this.onViewerJoined = options.onViewerJoined || (() => {});

      this.ws = null;
      this.pc = null;
      this.stream = null;
      this.pin = null;
      this.pendingCandidates = [];
      this.remoteDescSet = false;
      this.closed = false;
    }

    async start() {
      this.closed = false;
      this._setStatus('connecting');

      let stream;
      try {
        stream = await navigator.mediaDevices.getDisplayMedia({
          video: { frameRate: { ideal: 30, max: 60 } },
          audio: false,
        });
      } catch (err) {
        this._setStatus('error');
        this.onError('capture_denied');
        return;
      }
      this.stream = stream;

      const [track] = stream.getVideoTracks();
      if (track) {
        track.addEventListener('ended', () => {
          this.onEnded();
          this.stop();
        });
      }

      try {
        await this._openSocket();
      } catch {
        this._setStatus('error');
        this.onError('signaling_failed');
        this._stopStreamOnly();
        return;
      }

      this._send({ type: 'create-session' });
      this._setStatus('waiting');
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
        case 'session-created':
          this.pin = msg.pin;
          this.onPin(msg.pin);
          break;

        case 'viewer-joined':
          this._setStatus('connecting');
          this.onViewerJoined();
          await this._createOffer();
          break;

        case 'answer':
          await this._handleAnswer(msg);
          break;

        case 'ice-candidate':
          await this._handleRemoteIce(msg);
          break;

        case 'peer-disconnected':
          this._setStatus('disconnected');
          this.onError('peer_disconnected');
          this.stop();
          break;

        case 'session-expired':
          this.onError('pin_expired');
          this.stop();
          break;

        case 'error':
          this.onError(msg.error || 'failed');
          this._setStatus('error');
          break;
      }
    }

    async _createOffer() {
      const pc = new RTCPeerConnection({ iceServers: this.iceServers });
      this.pc = pc;

      pc.addEventListener('icecandidate', (ev) => {
        if (ev.candidate) {
          this._send({ type: 'ice-candidate', candidate: ev.candidate });
        }
      });

      pc.addEventListener('connectionstatechange', () => {
        const state = pc.connectionState;
        if (state === 'connected') this._setStatus('streaming');
        else if (state === 'disconnected') this._setStatus('connecting');
        else if (state === 'failed') {
          this._setStatus('error');
          this.onError('connection_failed');
        } else if (state === 'closed') {
          this._setStatus('disconnected');
        }
      });

      for (const track of this.stream.getTracks()) {
        pc.addTrack(track, this.stream);
      }

      // Prefer VP9 codec if available
      try {
        const transceivers = pc.getTransceivers();
        for (const t of transceivers) {
          if (t.sender && t.sender.track && t.sender.track.kind === 'video') {
            const capabilities = RTCRtpSender.getCapabilities
              ? RTCRtpSender.getCapabilities('video')
              : null;
            if (capabilities && capabilities.codecs) {
              const preferred = [];
              const others = [];
              for (const c of capabilities.codecs) {
                if (/VP9/i.test(c.mimeType)) preferred.push(c);
                else others.push(c);
              }
              if (preferred.length && typeof t.setCodecPreferences === 'function') {
                t.setCodecPreferences([...preferred, ...others]);
              }
            }
          }
        }
      } catch {
        /* codec preference is best-effort */
      }

      try {
        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        this._send({ type: 'offer', sdp: offer });
      } catch (err) {
        this.onError('negotiation_failed');
      }
    }

    async _handleAnswer(msg) {
      if (!this.pc) return;
      try {
        await this.pc.setRemoteDescription(new RTCSessionDescription(msg.sdp || msg));
        this.remoteDescSet = true;
        for (const c of this.pendingCandidates) {
          try { await this.pc.addIceCandidate(c); } catch { /* ignore */ }
        }
        this.pendingCandidates = [];
      } catch {
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
        /* ignore */
      }
    }

    _setStatus(status) {
      this.status = status;
      this.onStatus(status);
    }

    _stopStreamOnly() {
      if (this.stream) {
        for (const t of this.stream.getTracks()) {
          try { t.stop(); } catch { /* ignore */ }
        }
        this.stream = null;
      }
    }

    stop() {
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
      this._stopStreamOnly();
      this.pin = null;
      this._setStatus('idle');
    }
  }

  window.GhostViewHostLite = GhostViewHostLite;
})();

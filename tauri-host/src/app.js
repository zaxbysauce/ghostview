// GhostView Pro — frontend controller.
//
// Talks to the Tauri backend via window.__TAURI__.core.invoke and listens for
// `session-status` events emitted by the Rust side.

const invoke = (cmd, args) =>
  window.__TAURI__?.core?.invoke
    ? window.__TAURI__.core.invoke(cmd, args)
    : Promise.reject(new Error("Tauri runtime not available"));

const listen = (event, handler) =>
  window.__TAURI__?.event?.listen
    ? window.__TAURI__.event.listen(event, handler)
    : Promise.resolve(() => {});

const els = {
  monitorSelect: document.getElementById("monitor-select"),
  startBtn: document.getElementById("start-btn"),
  stopBtn: document.getElementById("stop-btn"),
  statusDot: document.getElementById("status-dot"),
  statusText: document.getElementById("status-text"),
  pinPanel: document.getElementById("pin-panel"),
  pinDisplay: document.getElementById("pin-display"),
  pinCountdown: document.getElementById("pin-countdown"),
  copyBtn: document.getElementById("copy-pin"),
  errorBanner: document.getElementById("error-banner"),
};

const state = {
  running: false,
  pin: null,
  expiresAt: null,
  countdownTimer: null,
};

function setStatus(kind, text) {
  const valid = ["idle", "connecting", "waiting", "streaming", "error"];
  els.statusDot.className = "status-dot " + (valid.includes(kind) ? kind : "idle");
  els.statusText.textContent = text;
}

function showError(msg) {
  els.errorBanner.textContent = msg;
  els.errorBanner.classList.remove("hidden");
  setStatus("error", "Error");
}

function clearError() {
  els.errorBanner.textContent = "";
  els.errorBanner.classList.add("hidden");
}

function setPin(pin, expiresInSec = 600) {
  state.pin = pin;
  state.expiresAt = Date.now() + expiresInSec * 1000;
  els.pinDisplay.textContent = pin;
  els.pinPanel.classList.remove("hidden");
  startCountdown();
}

function clearPin() {
  state.pin = null;
  state.expiresAt = null;
  els.pinDisplay.textContent = "------";
  els.pinPanel.classList.add("hidden");
  if (state.countdownTimer) {
    clearInterval(state.countdownTimer);
    state.countdownTimer = null;
  }
}

function startCountdown() {
  if (state.countdownTimer) clearInterval(state.countdownTimer);
  const tick = () => {
    if (!state.expiresAt) {
      els.pinCountdown.textContent = "--:--";
      return;
    }
    const remaining = Math.max(0, state.expiresAt - Date.now());
    const totalSec = Math.floor(remaining / 1000);
    const mm = String(Math.floor(totalSec / 60)).padStart(2, "0");
    const ss = String(totalSec % 60).padStart(2, "0");
    els.pinCountdown.textContent = `${mm}:${ss}`;
    if (remaining <= 0) {
      clearInterval(state.countdownTimer);
      state.countdownTimer = null;
    }
  };
  tick();
  state.countdownTimer = setInterval(tick, 1000);
}

async function loadMonitors() {
  try {
    const monitors = await invoke("list_monitors");
    els.monitorSelect.innerHTML = "";
    for (const m of monitors) {
      const opt = document.createElement("option");
      opt.value = String(m.index);
      opt.textContent = `${m.name} (${m.width}x${m.height})${
        m.is_primary ? " — primary" : ""
      }`;
      els.monitorSelect.appendChild(opt);
    }
  } catch (e) {
    showError(`Failed to list monitors: ${e}`);
  }
}

async function onStart() {
  clearError();
  els.startBtn.disabled = true;
  setStatus("connecting", "Connecting");
  const monitorIndex = parseInt(els.monitorSelect.value || "0", 10);
  try {
    const pin = await invoke("start_session", { monitorIndex });
    state.running = true;
    setPin(pin);
    setStatus("waiting", "Waiting for viewer");
    els.stopBtn.disabled = false;
  } catch (e) {
    showError(`Failed to start: ${e}`);
    els.startBtn.disabled = false;
  }
}

async function onStop() {
  els.stopBtn.disabled = true;
  try {
    await invoke("stop_session");
  } catch (e) {
    showError(`Failed to stop: ${e}`);
  } finally {
    state.running = false;
    clearPin();
    setStatus("idle", "Idle");
    els.startBtn.disabled = false;
  }
}

async function onCopy() {
  if (!state.pin) return;
  try {
    await navigator.clipboard.writeText(state.pin);
    const original = els.copyBtn.textContent;
    els.copyBtn.textContent = "Copied!";
    setTimeout(() => {
      els.copyBtn.textContent = original;
    }, 1200);
  } catch (e) {
    showError(`Clipboard error: ${e}`);
  }
}

function wireEvents() {
  els.startBtn.addEventListener("click", onStart);
  els.stopBtn.addEventListener("click", onStop);
  els.copyBtn.addEventListener("click", onCopy);

  listen("session-status", (event) => {
    const payload = event?.payload || {};
    if (payload.state) {
      const label =
        {
          idle: "Idle",
          connecting: "Connecting",
          waiting: "Waiting for viewer",
          streaming: "Streaming",
          error: "Error",
        }[payload.state] || payload.state;
      setStatus(payload.state, label);
    }
    if (payload.pin) setPin(payload.pin);
    if (payload.message) {
      if (payload.state === "error") showError(payload.message);
    }
  }).catch(() => {
    /* not running under Tauri */
  });
}

async function init() {
  wireEvents();
  setStatus("idle", "Idle");
  await loadMonitors();
}

document.addEventListener("DOMContentLoaded", init);

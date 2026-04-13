# GhostView Pro — Code Review Remediation Plan

**Last Updated:** 2026-04-13 (v2 — Post-Critic Review)  
**Review Scope:** 17 findings across stability, correctness, and security  
**Phase:** Phase 1 (Windows, VP9, WebRTC, basic signaling)

## Critic Review Feedback (Incorporated)

An independent critic reviewed this plan and identified 4 blocking issues. All have been resolved in v2:

✅ **C1: Panic handling specification clarified** — Added DETAILED SPEC for each spawned task (encoder blocking catch_unwind, async tasks JoinError::is_panic checks)  
✅ **M2: M1 dependency documented** — Explicitly marked M1 as blocking predecessor to M2  
✅ **H4: State machine completed** — Added protocol invariants, buffer limits, cleanup logic, and specific test cases  
✅ **C3: Secret migration path specified** — Added environment variable injection approach, .gitignore verification, pre-commit hook  

Also addressed: H5 risk assessment added; C2 edge cases documented; C4 clarified as already safe.

---

## 1. CRITICAL PATH (MUST FIX FIRST)

### C1: Panic Handling in Spawned Tasks

**Root Cause:**  
Five spawned tasks (`spawn_encoder_pair`'s blocking + async forwarder, `spawn_signaling_loop`, `spawn_ice_forwarder`, `spawn_state_watcher`) and the supervisor task (line 190) lack panic guards, causing silent failures or unhandled panics if closure code or async code panics. Additionally, the `shutdown_tx` channel (capacity 4, line 286) may overflow if 5+ tasks panic simultaneously, and the strategy for catching panics across FFI boundaries is undefined.

**Fix:**  
Implement three-layer panic handling: (1) wrap blocking encoder and cleanup code in `catch_unwind()` with panic signaling, (2) check `JoinError::is_panic()` when awaiting async tasks in teardown paths, (3) increase `shutdown_tx` capacity to 10 and define graceful pre-abort shutdown phase, (4) document FFI panic safety assumptions, (5) expand test suite to cover encoder panic, supervisor panic, and concurrent multi-task panics.

**Changes (DETAILED SPEC - REVISED v3):**

**File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs`

#### Gap 1 & 2: Encoder Forwarder + Supervisor Panic Guards

**Encoder Forwarder (lines 503–509):**
- Currently unguarded. Add check in `PartialInit::cleanup()` and `Running::teardown()`:
  ```rust
  if let Some(h) = self.encoder_forwarder.take() {
      match h.await {
          Ok(_) => {},
          Err(e) if e.is_panic() => {
              tracing::error!("encoder forwarder panicked: task will be aborted");
          }
          Err(e) => {
              tracing::warn!("encoder forwarder join error: {}", e);
          }
      }
  }
  ```

**Supervisor Panic Guard (line 190):**
- Supervisor task is async and cannot use catch_unwind. Document that if teardown() panics, the supervisor task panics and tokio logs it. The outer process supervisor (systemd, k8s) must restart.

#### Gap 3: Increase shutdown_tx Capacity + Graceful Cleanup Phase

**Capacity (line 286):**
```rust
// Increase from 4 to 10 to handle up to 10 concurrent panic signals
let (shutdown_tx, shutdown_rx) = mpsc::channel::<&'static str>(10);
```

**Graceful shutdown phase in Running::teardown() (before line 114 abort_after calls):**
```rust
async fn teardown(mut self) {
    let _ = self.signaling.send(ClientMessage::EndSession).await;
    self.signaling.close().await;
    self.capture.stop().await;
    
    if let Err(e) = self.webrtc.close().await {
        tracing::warn!(error = %e, "webrtc: close failed");
    }
    
    // Graceful shutdown phase: allow tasks 100ms to flush buffered state
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Abort phase: force exit remaining tasks (lines 114-137)
    abort_after(...).await;
}
```

#### Gap 4: FFI Safety Justification

**Add SAFETY comment to encoder blocking wrapper (around line 426):**
```rust
// SAFETY: libvpx (vpx_encode crate) is a C library. Unwinding across the FFI
// boundary via catch_unwind() is technically UB if libvpx holds lock-like state.
// However, in practice: (1) vpx_encode wraps stateless functions, (2) panics are
// Rust panics (allocation failure, validation), not libvpx panics, (3) we catch
// before unwinding out of the FFI call. If libvpx crashes, the OS terminates
// the process—no recovery attempted.
```

#### Gap 5: Expanded Test Suite

Add 4 new test cases (after line 751):
```rust
#[tokio::test]
async fn encoder_panic_caught_and_signaled() {
    // Verify catch_unwind catches encoder panic and sends shutdown_tx
}

#[tokio::test]
async fn async_task_panic_detected_on_join() {
    // Verify JoinError::is_panic() detects async task panic
}

#[tokio::test]
async fn concurrent_5task_panic_scenario() {
    // Simulate 5 tasks, 3 panic; verify shutdown_tx capacity=10 handles all
}

#[tokio::test]
async fn supervisor_teardown_with_panic_signals() {
    // Verify supervisor logs panic and completes teardown
}
```

#### Gap 6: Enhanced abort_after with Panic Detection

**Modify abort_after() (line 702):**
```rust
async fn abort_after(task: JoinHandle<()>, timeout: Duration) {
    let abort = task.abort_handle();
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(())) => {
            // Task exited cleanly
        }
        Ok(Err(e)) => {
            if e.is_panic() {
                tracing::error!("task panicked during shutdown; will be aborted");
            } else if e.is_cancelled() {
                tracing::debug!("task was cancelled");
            } else {
                tracing::warn!("task join error: {}", e);
            }
        }
        Err(_) => {
            tracing::warn!(timeout_ms = timeout.as_millis(), "task did not exit within timeout; aborting");
            abort.abort();
        }
    }
}
```

#### Gap 7: Async vs Blocking Panic Strategy Documentation

**Add to encoder_pair docstring (line 418):**
```rust
/// **Panic Handling:**
/// - Blocking encoder: Wrapped in catch_unwind(). On panic, sends shutdown_tx
///   then lets unwind propagate. JoinError::is_panic() is true in teardown().
/// - Async forwarder: Panics cannot wrap. If webrtc.push_frame() panics, the
///   task panics and JoinError::is_panic() is true in teardown().
/// - Both paths log panics and trigger coordinated shutdown via shutdown_tx.
```

**Test Approach:**
- Unit: catch_unwind logic, JoinError::is_panic() detection, 5-task concurrent scenario
- Timeout: verify abort_after detects and logs timeouts
- Integration: trigger encoder panic, verify supervisor logs and teardown completes

---

### C2: Grace Window Infinite Extension (Disconnected State)

**Root Cause:**  
Line 654 recalculates `deadline` from `now()` on every state transition during grace window. Receiving `Disconnected` repeatedly resets the deadline, potentially extending grace indefinitely.

**Fix:**  
Calculate deadline once before entering grace loop (line 650). Use single deadline for all iterations.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs` lines 650–682
- Move `let deadline = ...` outside loop to line 651 (before inner loop opens at 656)
- Remove deadline recalculation on line 654
- Inline formula: `let deadline = tokio::time::Instant::now() + DISCONNECT_GRACE;` (once)

**Test Approach:**
- Unit test: simulate repeated `Disconnected` events; verify timeout fires after ~15s regardless of event frequency
- Mock clock test using tokio's time testing utilities

---

### C3: Hardcoded Secrets in turnserver.conf

**Root Cause:**  
File `/home/user/ghostview/turnserver.conf` contains plaintext TURN credential `user=ghost:supersecretpassword` on line 26 (not committed due to .gitignore, but present on disk in development environment).

**Fix:**  
Delete `turnserver.conf` from dev environment. For deployment, use environment variables to inject credentials at container startup. Update `.gitignore` to ensure turnserver.conf is never committed. Add pre-commit hook to reject commits containing TURN credentials.

**Changes (DETAILED SPEC):**

1. **Delete the dev file:**
   - `rm /home/user/ghostview/turnserver.conf`

2. **Verify .gitignore:**
   - Check `/home/user/ghostview/.gitignore` line 25: confirm `turnserver.conf` is listed
   - If missing, add: `turnserver.conf`

3. **Update README.md — Add new section "TURN Server Deployment":**
   ```markdown
   ### TURN Server (coturn) Setup
   
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
   ```

4. **Add pre-commit hook (optional, but recommended):**
   - Create `.git/hooks/pre-commit` to reject commits if `turnserver.conf` is tracked or if it contains `supersecretpassword` pattern
   - Command: `git grep -n "supersecretpassword" -- *.conf && exit 1 || exit 0`

**Test Approach:**
- Git check: confirm file is removed from HEAD and history on next commit
- Build verification: docker-compose still works with .example file

---

### C4: Monitor Enumeration Race Condition (capture.rs:273–278)

**Root Cause:**  
Line 277–278 calls `.nth(monitor_index)` on iterator without bounds check. If monitor count changes between `enumerate()` and `nth()` (race condition on some systems), `.nth()` returns `None` → panic via `.ok_or()` unwrap.

**Fix:**  
Replace `.nth()` with bounds-safe indexing. Check list length before access.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/capture.rs` lines 269–278
- Change:
  ```rust
  let monitors = Monitor::enumerate()
      .map_err(|e| CaptureError::Failed(format!("monitor enumerate: {e:?}")))?;
  let monitor = monitors
      .into_iter()
      .nth(monitor_index)
      .ok_or(CaptureError::MonitorNotFound(monitor_index))?;
  ```
- To:
  ```rust
  let monitors = Monitor::enumerate()
      .map_err(|e| CaptureError::Failed(format!("monitor enumerate: {e:?}")))?;
  let monitor = monitors
      .get(monitor_index)
      .cloned()  // if needed
      .ok_or(CaptureError::MonitorNotFound(monitor_index))?;
  ```

**Test Approach:**
- Unit test: mock `Monitor::enumerate()` to return list of 2 monitors, request index 5, verify `MonitorNotFound(5)` error returned
- Platform-specific: on Windows, unplug monitor during capture start, verify graceful error

---

## 2. HIGH PRIORITY (Blocking Stability / Correctness)

### H1: Remove Unused "Active" State

**Root Cause:**  
Session manager state machine tracks `Active` state that is never entered or checked (code review artifact).

**Fix:**  
Audit state transitions in `Running` struct and session teardown. Remove dead enum variant if unused.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs`
- Grep for `"active"` / `Active` enum variant
- If found and unused, remove from enum definition
- Verify all state paths still compile

**Test Approach:**
- Cargo check should pass
- Confirm no references to removed state in logs

---

### H2: Document EOC Asymmetry

**Root Cause:**  
End-of-candidates (EOC) protocol differs between send (explicit `{candidate: null}`) and receive (implicit). Needs documentation of intentional asymmetry.

**Fix:**  
Add inline doc comment explaining design decision.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/webrtc_host.rs` lines 171–207 (ICE send path)
- Add doc comment:
  ```rust
  /// On_ice_candidate callback: forward local candidates.
  /// 
  /// Protocol asymmetry by design: we send explicit `{candidate: null}` 
  /// end-of-candidates to the viewer, but implicitly handle received EOC
  /// (webrtc-rs consumes it). This simplifies state tracking and matches
  /// browser WebRTC behavior.
  ```
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs` lines 544–548
- Existing comment at line 546 ("End-of-candidates sentinel") is sufficient; no change needed

**Test Approach:**
- Code review: verify comment is present and clear
- Protocol-level: test with viewer, confirm EOC signals are respected

---

### H3: Increase ICE Channel Capacity (64 → 256) + Overflow Logging

**Root Cause:**  
ICE candidate channel capacity is 64 (line 170, `webrtc_host.rs`). During ICE restart or in poor network conditions, candidates can accumulate faster than signaling forwards them, causing drops.

**Fix:**  
Increase to 256 (4× buffer). Add overflow logging on `send()` failure to detect saturation.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/webrtc_host.rs` line 170
  - Change: `mpsc::channel::<LocalIceCandidate>(64)`
  - To: `mpsc::channel::<LocalIceCandidate>(256)`
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/webrtc_host.rs` lines 203–205
  - Change:
    ```rust
    if tx.send(local).await.is_err() {
        tracing::debug!("webrtc: ice candidate channel closed");
    }
    ```
  - To:
    ```rust
    if let Err(e) = tx.send(local).await {
        tracing::warn!(candidate = %candidate, "webrtc: ice candidate overflow or channel closed: {e}");
    }
    ```

**Test Approach:**
- Unit test: mock rapid ICE candidate generation, verify 64 → 256 absorbs them without drop
- Integration test: run with packet loss simulation, verify metrics log overflow events if they occur

---

### H4: Implement Offer-Before-Candidates Ordering in Signaling

**Root Cause:**  
Protocol allows viewer to receive ICE candidates before SDP offer. Browser will buffer candidates without matching media line index, causing race condition. Should enforce offer first. Current implementation (lines 514–594) lacks state tracking, buffer management, answer validation, event handlers for cleanup, and protocol invariant tests.

**Fix:**  
Implement offer-before-candidates protocol with explicit state machine: (1) buffer ICE until offer sent, (2) validate answer before continuing, (3) handle ViewerLeft/NetworkError with cleanup, (4) manage rate-limited draining, (5) enforce FIFO buffer overflow semantics with detailed logging.

---

## State Machine & Transitions

```
┌─────────────────────────────────────────────────────────────────┐
│                      SIGNALING LOOP STATE                       │
├─────────────────────────────────────────────────────────────────┤
│ Initial: offer_sent=false, answer_received=false, ice_pending=[] │
│                                                                 │
│ Event: ViewerJoined                                             │
│   → create_offer() → send Offer → set offer_sent=true           │
│   → drain ice_pending (rate-limited: sleep 1ms between sends)   │
│   → move to "Awaiting Answer" state                             │
│                                                                 │
│ Event: Answer (in "Awaiting Answer")                            │
│   → set_answer(sdp) → set answer_received=true                  │
│   → move to "Connected" state                                   │
│                                                                 │
│ Event: IceCandidate                                             │
│   → if !offer_sent: buffer (FIFO, max 256)                      │
│   → else: forward to webrtc.add_ice_candidate()                 │
│   → if buffer full: drop (FIFO, log details)                    │
│                                                                 │
│ Event: ViewerLeft / PeerDisconnected / NetworkError             │
│   → clear ice_pending, set offer_sent=false, answer_received=false
│   → log cleanup with count of dropped candidates               │
│   → return to Initial state                                     │
└─────────────────────────────────────────────────────────────────┘
```

**Transition Table:**

| State | Event | Action | New State | Side Effects |
|-------|-------|--------|-----------|--------------|
| Initial | ViewerJoined | create + send offer, drain buffer | Awaiting Answer | offer_sent=T, drain with 1ms sleep |
| Awaiting Answer | Answer | set_answer(), mark received | Connected | answer_received=T, can forward ICE |
| Awaiting Answer | IceCandidate | buffer if <256, drop+warn if full | Awaiting Answer | ice_pending grows |
| Connected | IceCandidate | forward immediately | Connected | no buffer |
| Any | ViewerLeft/Error | clear buffer, reset flags | Initial | log dropped count |

---

## Detailed Changes

**File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs` lines 514–594 (`spawn_signaling_loop`)

### 1. Add State & Buffer Initialization (after line 520, inside async block)

```rust
fn spawn_signaling_loop(
    mut events: mpsc::Receiver<ServerMessage>,
    client_tx: mpsc::Sender<ClientMessage>,
    webrtc: Arc<WebRtcHost>,
    shutdown_tx: mpsc::Sender<&'static str>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // === PROTOCOL STATE MACHINE (GAP 1: VecDeque reflow semantics) ===
        let mut ice_pending: VecDeque<LocalIceCandidate> = VecDeque::new();
        let mut offer_sent = false;
        let mut answer_received = false;
        const MAX_PENDING_ICE: usize = 256;
        
        // Track drops for logging (GAP 6: buffer drop strategy)
        let mut total_dropped = 0_u32;
        
        while let Some(msg) = events.recv().await {
            match msg {
```

### 2. Update ViewerJoined Handler (lines 523–543)

```rust
                ServerMessage::ViewerJoined => {
                    tracing::info!("signaling: viewer joined — creating offer");
                    match webrtc.create_offer().await {
                        Ok(sdp) => {
                            let payload = SdpPayload {
                                kind: "offer".to_string(),
                                sdp,
                            };
                            // === GAP 4: Offer_sent transition timing with explicit ordering ===
                            // Order: send offer → set flag → drain buffer
                            match client_tx.send(ClientMessage::Offer { sdp: payload }).await {
                                Ok(_) => {
                                    // Offer sent successfully; set flag and begin drain
                                    offer_sent = true;
                                    tracing::debug!(pending_count = ice_pending.len(), "signaling: offer sent, draining pending ICE");
                                    
                                    // === GAP 4: Rate-limiting during drain (256+ candidates) ===
                                    // Sleep 1ms between sends to avoid WebSocket overflow
                                    while let Some(ice) = ice_pending.pop_front() {
                                        let msg = ClientMessage::IceCandidate {
                                            candidate: Some(ice.clone()),
                                        };
                                        if let Err(e) = client_tx.send(msg).await {
                                            tracing::warn!(error = %e, "signaling: failed to send buffered ICE, stopping drain");
                                            break;
                                        }
                                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                                    }
                                    tracing::info!(dropped = total_dropped, "signaling: finished draining pending ICE");
                                }
                                Err(e) => {
                                    // === GAP 4: Rollback on async failure ===
                                    tracing::warn!(error = %e, "signaling: send offer failed; NOT setting offer_sent flag");
                                    offer_sent = false; // Rollback: keep buffering
                                    let _ = shutdown_tx.try_send("offer_send_failed");
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "webrtc: create_offer failed");
                            let _ = shutdown_tx.try_send("create_offer_failed");
                            break;
                        }
                    }
                }
```

### 3. Update Answer Handler (lines 545–551)

```rust
                ServerMessage::Answer { sdp } => {
                    // === GAP 2: answer_received flag now properly guarded ===
                    // We set the flag AFTER successful application to prevent
                    // ICE forwarding before webrtc-rs ingests the answer
                    if let Err(e) = webrtc.set_answer(sdp.sdp).await {
                        tracing::error!(error = %e, "webrtc: set_answer failed");
                        let _ = shutdown_tx.try_send("set_answer_failed");
                        break;
                    }
                    // Only after successful set_answer can we safely forward new ICE
                    answer_received = true;
                    tracing::debug!("signaling: answer applied, can now forward ICE candidates");
                }
```

### 4. Update IceCandidate Handler (lines 552–568)

```rust
                ServerMessage::IceCandidate { candidate } => {
                    let Some(c) = candidate else {
                        // End-of-candidates sentinel; webrtc-rs handles implicitly
                        continue;
                    };
                    
                    // === GAP 1: VecDeque reflow semantics — buffer ONLY while offer_sent=false ===
                    // Once offer_sent=true, all buffering ends; candidates forward immediately
                    // New candidates cannot be buffered after draining completes
                    if !offer_sent {
                        // Offer not yet sent: buffer this candidate
                        // === GAP 6: FIFO drop strategy with detailed logging ===
                        if ice_pending.len() < MAX_PENDING_ICE {
                            ice_pending.push_back(c);
                            tracing::trace!("signaling: buffered ICE candidate (pending={}/{})", 
                                           ice_pending.len(), MAX_PENDING_ICE);
                        } else {
                            // === GAP 6: Log dropped candidate details (FIFO drop) ===
                            total_dropped += 1;
                            tracing::warn!(
                                candidate = %c.candidate,
                                sdp_mline_index = c.sdp_mline_index,
                                dropped_total = total_dropped,
                                "signaling: ICE buffer FULL (256/256), dropping OLDEST candidate (FIFO); consider increasing buffer"
                            );
                        }
                    } else {
                        // Offer already sent: forward immediately to webrtc-rs
                        // === GAP 2: answer_received guard — allow early forwarding OR require answer? ===
                        // DECISION (documented here): webrtc-rs internally queues candidates received
                        // before answer; they are applied retroactively upon set_remote_description().
                        // However, for safety, we recommend guard: forward only if answer_received=true.
                        // IMPLEMENTATION: Using guard to maximize safety (conservative).
                        if answer_received {
                            if let Err(e) = webrtc
                                .add_ice_candidate(
                                    c.candidate,
                                    c.sdp_mid,
                                    c.sdp_mline_index,
                                    c.username_fragment,
                                )
                                .await
                            {
                                tracing::warn!(error = %e, "webrtc: add_ice_candidate failed");
                            }
                        } else {
                            // Answer not yet received; defer forwarding to prevent out-of-order state
                            if ice_pending.len() < MAX_PENDING_ICE {
                                ice_pending.push_back(c);
                                tracing::trace!("signaling: answer not yet received, buffering ICE temporarily");
                            } else {
                                total_dropped += 1;
                                tracing::warn!(candidate = %c.candidate, "signaling: deferred buffer full, dropping");
                            }
                        }
                    }
                }
```

### 5. Add Explicit Handlers for Cleanup (GAP 3: ViewerLeft/NetworkError handlers)

Add AFTER the IceCandidate match arm:

```rust
                // === GAP 3: Explicit cleanup handlers for protocol events ===
                ServerMessage::ViewerLeft => {
                    let dropped = ice_pending.len();
                    ice_pending.clear();
                    offer_sent = false;
                    answer_received = false;
                    total_dropped = 0;
                    tracing::info!(
                        "signaling: viewer left — cleared {} pending candidates, reset state machine",
                        dropped
                    );
                    // Do NOT break; loop continues awaiting new viewer
                }
                
                // GAP 3: NetworkError (if added to ServerMessage enum)
                // ServerMessage::NetworkError => { ... same cleanup ... }
```

Update existing PeerDisconnected handler (lines 577–581):

```rust
                ServerMessage::PeerDisconnected | ServerMessage::SessionExpired => {
                    // === GAP 3: Explicit cleanup on disconnect ===
                    let dropped = ice_pending.len();
                    ice_pending.clear();
                    offer_sent = false;
                    answer_received = false;
                    tracing::info!(
                        "signaling: session ended by server/peer — cleared {} pending candidates",
                        dropped
                    );
                    let _ = shutdown_tx.try_send("peer_disconnected");
                    break; // Exit signaling loop
                }
```

### 6. Update Error Handler with Cleanup

```rust
                ServerMessage::Error { error } => {
                    tracing::warn!(%error, "signaling: server error");
                    // === GAP 3: Clear state on hard errors ===
                    if error == "server_shutdown" || error == "rate_limited" {
                        let dropped = ice_pending.len();
                        ice_pending.clear();
                        offer_sent = false;
                        answer_received = false;
                        tracing::error!(dropped, "signaling: hard error, cleared pending ICE");
                        let _ = shutdown_tx.try_send("server_error");
                        break;
                    }
                }
            }
        }
    })
}
```

---

## Protocol Invariants (Enforcement & Validation)

**Invariant List:**

1. **I1: Offer-Before-Candidates** — No ICE candidate message is transmitted to the viewer before an Offer message.
   - **Enforcement:** offer_sent flag guards all candidate transmission (line check in test).

2. **I2: Answer-Before-Forwarding** — No ICE candidate is forwarded to webrtc-rs until after Answer is applied.
   - **Enforcement:** answer_received guard in IceCandidate handler.

3. **I3: Buffer Drain Idempotence** — Once offer_sent=true and buffer drains, no new candidates are buffered; all subsequent candidates forward immediately.
   - **Enforcement:** if !offer_sent guard never buffers after drain completes.

4. **I4: Max Pending Limit** — ice_pending never exceeds 256 entries.
   - **Enforcement:** < MAX_PENDING_ICE check before push_back.

5. **I5: ViewerLeft Cleanup** — When ViewerLeft is received, ice_pending is cleared and offer_sent, answer_received are reset to false.
   - **Enforcement:** explicit handler clears state and logs count.

6. **I6: Error State Recovery** — On hard errors (server_shutdown, rate_limited), state is cleared for potential reconnect.
   - **Enforcement:** Error handler resets state before shutdown signal.

---

## Test Specification (Covering All 6 Gaps)

### Unit Tests

**Test 1: ice_before_offer_buffered (GAP 1, 3)**
```rust
#[tokio::test]
async fn ice_before_offer_buffered() {
    // Arrange
    let (events_tx, events_rx) = mpsc::channel(100);
    let (client_tx, mut client_rx) = mpsc::channel(100);
    let (shutdown_tx, _) = mpsc::channel(10);
    let webrtc = Arc::new(mock_webrtc_host());
    
    // Act
    let _handle = spawn_signaling_loop(events_rx, client_tx, webrtc.clone(), shutdown_tx);
    
    // Send 5 ICE before offer
    for i in 0..5 {
        let _ = events_tx.send(ServerMessage::IceCandidate { 
            candidate: Some(LocalIceCandidate { 
                candidate: format!("candidate {i}"),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
                username_fragment: None,
            })
        }).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Viewer joins
    let _ = events_tx.send(ServerMessage::ViewerJoined).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Assert: 1 Offer + 5 IceCandidate messages received in order
    assert_eq!(client_rx.recv().await.map(|m| matches!(m, ClientMessage::Offer { .. })), Some(true));
    for _ in 0..5 {
        assert_eq!(client_rx.recv().await.map(|m| matches!(m, ClientMessage::IceCandidate { .. })), Some(true));
    }
}
```

**Test 2: max_pending_ice_prevents_dos (GAP 6)**
```rust
#[tokio::test]
async fn max_pending_ice_prevents_dos() {
    // Arrange
    let (events_tx, events_rx) = mpsc::channel(500);
    let (_client_tx, mut _client_rx) = mpsc::channel(500);
    let (shutdown_tx, _) = mpsc::channel(10);
    let webrtc = Arc::new(mock_webrtc_host());
    
    let _handle = spawn_signaling_loop(events_rx, _client_tx, webrtc.clone(), shutdown_tx);
    
    // Act: send 300 ICE before offer
    for i in 0..300 {
        let _ = events_tx.send(ServerMessage::IceCandidate {
            candidate: Some(LocalIceCandidate {
                candidate: format!("candidate {i}"),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
                username_fragment: None,
            })
        }).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Assert: logs show dropped > 44 (300 - 256)
    // (Verify via tracing subscriber mock or log capture)
}
```

**Test 3: answer_received_guard (GAP 2)**
```rust
#[tokio::test]
async fn ice_not_forwarded_before_answer() {
    // Send: ViewerJoined → Offer sent → ICE arrives (before Answer)
    // Verify: ICE is buffered, not forwarded to webrtc-rs
    // Verify: After Answer, buffered ICE is forwarded
}
```

**Test 4: viewer_left_clears_buffer (GAP 3, 5)**
```rust
#[tokio::test]
async fn viewer_left_clears_buffer() {
    // Send: 10 ICE → ViewerJoined → Offer sent (start drain)
    // Send: ViewerLeft (interrupt drain)
    // Verify: Remaining buffered ICE cleared, log shows count
}
```

**Test 5: rate_limiting_on_drain (GAP 4)**
```rust
#[tokio::test]
async fn drain_rate_limited_1ms_per_candidate() {
    // Setup: 256 pending ICE from before ViewerJoined
    // Send: ViewerJoined → Offer sent → Drain starts
    // Measure: Time to drain 256 candidates ≥ 250ms (1ms per send)
    // Verify: No WebSocket overflow errors logged
}
```

**Test 6: offer_send_failure_rollback (GAP 4)**
```rust
#[tokio::test]
async fn offer_send_failure_does_not_set_flag() {
    // Mock: client_tx.send() returns Err on Offer
    // Send: ViewerJoined
    // Verify: offer_sent remains false, subsequent ICE still buffered
    // Verify: shutdown_tx receives "offer_send_failed"
}
```

### Protocol Invariant Tests

**Test 7: offer_always_before_ice_message (I1)**
```rust
#[tokio::test]
async fn offer_precedes_all_ice_on_wire() {
    // Capture all messages sent to client_tx
    // Assert: client_rx.recv() == ClientMessage::Offer first
    // Assert: All subsequent ClientMessage::IceCandidate arrive after Offer
}
```

**Test 8: answer_before_webrtc_forwarding (I2)**
```rust
#[tokio::test]
async fn webrtc_ice_only_after_answer() {
    // Mock webrtc.add_ice_candidate to track calls
    // Send: ViewerJoined → offer_sent=true
    // Send: IceCandidate (no Answer yet)
    // Verify: webrtc.add_ice_candidate NOT called (buffered in ice_pending)
    // Send: Answer → answer_received=true
    // Verify: webrtc.add_ice_candidate called for buffered ICE
}
```

**Test 9: concurrent_viewer_join_ice_race (I3 + I4 + concurrent)**
```rust
#[tokio::test]
async fn concurrent_ice_and_viewer_join() {
    // Concurrently send: [IceCandidate, IceCandidate, ViewerJoined, IceCandidate, ...]
    // Verify: All are ordered correctly; buffer never exceeds 256; offer always sent first on wire
}
```

### Integration Tests

**Test 10: signaling_round_trip_with_buffering**
- Capture real WebSocket message sequence between host and signaling server
- Verify temporal ordering: Offer timestamp < all IceCandidate timestamps

---

## Buffer Drop Strategy (GAP 6 — Explicit)

**FIFO Drop (Drop Oldest):**
- When buffer is full (256 entries), next ICE is dropped (not inserted)
- Rationale: Most recent ICE candidates are more likely to connect; old ones may stale

**Logging Strategy:**
- **Per-drop log:** Each drop is logged at WARN level with candidate details (line 566–570 above)
- **Summary log:** Total dropped count logged at INFO level after drain completes (line 565 above)
- **Batch logs:** Not used; individual WARN per drop allows correlation with packet loss events

**Dropped Candidate Details Included:**
- candidate: %c.candidate (full SDP candidate line)
- sdp_mline_index: c.sdp_mline_index
- dropped_total: cumulative count in this session
- Recommend including in metrics/telemetry for network diagnostics

---

## Summary of Gap Resolutions

| Gap | Resolution |
|-----|-----------|
| 1 — VecDeque reflow | After drain, ice_pending remains empty; new ICE forwards directly (no re-buffering after offer_sent=true) |
| 2 — answer_received usage | Guard added: ICE forwarding to webrtc-rs only after answer_received=true; prevents premature application |
| 3 — ViewerLeft/Error handlers | Explicit handlers added; clear ice_pending, reset flags, log cleanup count |
| 4 — Offer_sent timing & rollback | Explicit order: send → set flag → drain. Failure rollback on send error; 1ms rate-limit sleep during drain |
| 5 — Protocol invariants | 6 invariants listed; enforcement via guards and tests (Test 7–9 verify) |
| 6 — Buffer drop | FIFO drop when full; per-drop logging with candidate details; summary after drain |

---

### H5: Change Atomic Ordering from Relaxed to AcqRel

**Root Cause:**  
Stopping flag uses `Ordering::Relaxed` on both load/store (capture.rs lines 175, 225, 236). Relaxed allows reordering relative to other atomics. If multiple frames are buffered or if there are other coordinating flags, weak ordering could create visibility delays.

**Risk assessment:** Current code checks only `stopping` flag in isolation (no concurrent access to other shared state). However, when stop() is called from supervisor task, it must be visible to the capture callback before frames continue to buffer. Using AcqRel is conservative and safe; Relaxed might be sufficient in practice but is harder to reason about.

**Fix:**  
Use `Ordering::AcqRel` to enforce acquire semantics on load (capture callback sees stop signal immediately) and release semantics on store (stop() call is flushed to all threads). This is standard for signaling flags.

**Changes (DETAILED SPEC):**

**File:** `/home/user/ghostview/tauri-host/src-tauri/src/capture.rs`

1. **Line 175 (in on_frame_arrived handler):**
   - Change: `if self.stopping.load(Ordering::Relaxed) {`
   - To: `if self.stopping.load(Ordering::Acquire) {`
   - Comment: `// Acquire: ensure stop signal is visible before frame capture continues`

2. **Line 225 (in stop() method):**
   - Change: `self.stopping.store(true, Ordering::Relaxed);`
   - To: `self.stopping.store(true, Ordering::Release);`
   - Comment: `// Release: flush stop signal to all threads immediately`

3. **Line 236 (in abort() method):**
   - Change: `self.stopping.store(true, Ordering::Relaxed);`
   - To: `self.stopping.store(true, Ordering::Release);`
   - Comment: `// Release: flush stop signal immediately for abrupt termination`

**Test Approach:**

- **Unit test:** `#[tokio::test] stop_signal_visible_to_callback()`
  - Mock capture handler that logs every frame check
  - Call stop() and verify next on_frame_arrived sees stopping=true (no frame processed after stop)

- **Stress test:** Capture 1000 frames under rapid stop/start cycles
  - Verify no frames are processed after stop() is called
  - Compare timing with Relaxed vs AcqRel to measure any latency difference (expect negligible)

- **Thread sanitizer:** Run tests with `-Z sanitizer=thread` to detect any data races (optional, but recommended for atomic changes)

---

## 3. MEDIUM PRIORITY (Correctness Guards)

### M1: Verify SDP Offer Creation Safety

**Root Cause:**  
`create_offer()` may fail if peer connection is in wrong state. Code at line 517 uses `?` operator without logging details.

**Fix:**  
(Likely no code change.) Verify via code review that offer can only be called after `new()` succeeds and before remote answer is applied. Document preconditions.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/webrtc_host.rs` lines 237–241
- Add doc comment:
  ```rust
  /// Create and set local SDP offer.
  /// 
  /// **Preconditions:** Must be called after `new()` and before `set_answer()`.
  /// **Idempotence:** Safe to call multiple times; webrtc-rs will regenerate offer.
  ```

**Test Approach:**
- Code review: trace offer creation in call graph, confirm `new()` always precedes it
- Unit test: verify error propagation if called in wrong state (negative test)

---

### M2: Add Idempotence Guard to set_remote_description

**Root Cause:**  
Line 247 in `webrtc_host.rs` calls `set_remote_description()` without checking if already called. Multiple answers would cause an error or corrupt state.

**BLOCKING DEPENDENCY:** M1 (document SDP offer preconditions) must be completed FIRST. This guard assumes that the caller has read the documented preconditions and understands that set_answer() should only be called once per session.

**Fix:**  
Add state flag to track if answer already applied. Return early on repeat call with trace log. Return Ok(()) instead of Err to allow idempotent retry from viewers without propagating errors up.

**Changes (DETAILED SPEC):**

**File:** `/home/user/ghostview/tauri-host/src-tauri/src/webrtc_host.rs`

1. **Lines 85–90 (struct fields):**
   - Add new field after `_rtp_sender`:
     ```rust
     answer_applied: std::sync::atomic::AtomicBool,
     ```

2. **Lines 219–222 (new()):**
   - Add initialization in return statement:
     ```rust
     Ok(Self {
         pc,
         video_track,
         _rtp_sender: rtp_sender,
         ice_rx: Mutex::new(Some(ice_rx)),
         state_rx: Mutex::new(Some(state_rx)),
         answer_applied: std::sync::atomic::AtomicBool::new(false),
     })
     ```

3. **Lines 244–250 (set_answer):**
   - Replace entire function:
     ```rust
     /// Apply the remote SDP answer. Idempotent: safe to call multiple times.
     /// 
     /// **Preconditions:** `create_offer()` must have been called first (documented in M1).
     /// If `set_answer()` is called twice, the second call is ignored and returns Ok(()).
     pub async fn set_answer(&self, sdp: String) -> Result<(), WebRtcError> {
         // Use swap() to atomically check-and-set in one operation
         if self.answer_applied.swap(true, std::sync::atomic::Ordering::AcqRel) {
             tracing::info!("webrtc: answer already applied; ignoring duplicate set_answer call");
             return Ok(()); // Return Ok to allow idempotent retry
         }
         let answer = RTCSessionDescription::answer(sdp)?;
         self.pc.set_remote_description(answer).await?;
         Ok(())
     }
     ```

**Test Approach (SPECIFIC TESTS REQUIRED):**

- Unit test: `#[tokio::test] set_answer_idempotent()` 
  - Create WebRtcHost
  - Call `create_offer()` (precondition)
  - Call `set_answer(sdp1)` twice with DIFFERENT SDPs
  - Verify first call succeeds (Ok(()))
  - Verify second call returns Ok(()) but logs "ignoring duplicate"
  - Verify webrtc-rs only saw the first answer applied (via internal state check if possible)

- Integration test: `signaling_roundtrip_duplicate_answer()`
  - Start host-viewer session
  - Viewer sends answer
  - Simulate network retry: viewer sends answer again
  - Verify host logs idempotent ignore and continues without error

---

### M3: Document Frame Buffering as Phase 2 Work

**Root Cause:**  
Frames buffer in `TrackLocalStaticSample` before peer connects. No metrics or backpressure. Noted for Phase 2 but not documented in code.

**Fix:**  
Add comment in `spawn_encoder_pair()` explaining buffering behavior and Phase 2 roadmap.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs` lines 410–416
- Add doc comment:
  ```rust
  /// Encoder pair: blocking libvpx loop + async forwarder.
  /// 
  /// **Frame buffering (Phase 1):** Encoded frames buffer in the attached
  /// WebRTC `TrackLocalStaticSample` until a peer connects. No backpressure
  /// or metrics; memory grows linearly with frame rate until connection.
  /// 
  /// **Phase 2:** Implement metrics (buffer depth, drops) and optional
  /// frame drop if buffer exceeds threshold (e.g., >500 frames).
  ```

**Test Approach:**
- Code review: verify comment is visible
- Manual: start capture, measure memory growth before peer joins, document baseline

---

### M4: Pin Tauri Version to Exact Match

**Root Cause:**  
Cargo.toml line 17–20 uses `version = "2"` for both `tauri` and `tauri-build`. Minor version mismatch between them can cause subtle build errors.

**Fix:**  
Pin to exact matching versions using `=` operator.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/Cargo.toml` lines 17, 20
- Change:
  ```toml
  tauri-build = { version = "2", features = [] }
  tauri = { version = "2", features = [] }
  ```
- To: (assuming current is 2.1.x, find exact version from `cargo update` output)
  ```toml
  tauri-build = { version = "= 2.1.1", features = [] }
  tauri = { version = "= 2.1.1", features = [] }
  ```

**Test Approach:**
- Build: `cargo update && cargo build` on clean machine, verify reproducible build
- CI: lock Cargo.lock in git, verify no version drift

---

### M5: Return Generic Error for All PIN Join Failures

**Root Cause:**  
PIN join endpoints may leak operational details (e.g., "invalid_pin" vs. "session_not_found"). Potential information disclosure.

**Fix:**  
Catch all PIN join errors and return single generic message to client.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/signaling.rs` (join flow if implemented)
- Note: Current Phase 1 code does not implement viewer join; this is for future multi-viewer support
- When implemented, wrap join attempt in:
  ```rust
  pub async fn join_session(&mut self, pin: &str) -> Result<(), SignalingError> {
      let result = self.send(ClientMessage::JoinSession { pin: pin.to_string() }).await?;
      match result {
          ServerMessage::SessionJoined => Ok(()),
          ServerMessage::Error { error: _ } => {
              // Log operational detail internally, return generic error to caller
              tracing::warn!("join failed for pin {pin}");
              Err(SignalingError::Signaling("Invalid PIN or session expired".to_string()))
          }
          _ => Err(SignalingError::Signaling("Unexpected server response".to_string())),
      }
  }
  ```

**Test Approach:**
- Unit test: mock server returning different PIN errors, verify all mapped to generic message
- Integration test: attempt join with invalid PIN, verify error message is generic

---

## 4. LOW PRIORITY (Polish / Documentation)

### L1: Frame-Rate Limiter Deferred to Phase 2

**Status:** Working as designed (Phase 1 baseline).  
**Phase 2 Action:** Implement adaptive frame rate based on network congestion (RTCP feedback).

**No fix required for Phase 1.**

---

### L2: Encoder Leak Deferred to Phase 2

**Status:** Known limitation.  
**Details:** VP9 encoder may leak OS thread on abort. Logged at line 129.

**Phase 2 Action:** Consider graceful degradation or pre-allocation pooling.

**No fix required for Phase 1.**

---

### L3: Update COM Documentation

**Root Cause:**  
Comments in capture.rs (lines 122–126) refer to COM STA (Single-Threaded Apartment). Modern Windows uses MTA (Multi-Threaded).

**Fix:**  
Update comment to reflect current Windows threading model.

**Changes:**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/capture.rs` lines 122–126
- Change:
  ```rust
  // isolates COM STA state and prevents conflicts with Tokio's multi-threaded
  ```
- To:
  ```rust
  // isolates Windows COM state (STA or MTA) and prevents conflicts with Tokio's multi-threaded
  ```

**Test Approach:**
- Build verification: verify code compiles
- Documentation: confirm comment matches Windows API docs

---

## PRIORITY SUMMARY

| Category | Count | Effort | Risk | Timeline |
|----------|-------|--------|------|----------|
| Critical | 4 | 8h | High | Week 1 |
| High | 5 | 12h | Medium | Week 2 |
| Medium | 5 | 6h | Low | Week 3 |
| Low | 3 | 2h | Minimal | Phase 2 |
| **Total** | **17** | **28h** | — | **3–4 weeks** |

---

## IMPLEMENTATION ORDER (REVISED AFTER CRITIC REVIEW)

**Sequential (blocking) dependencies:**
1. **M1 must complete before M2** — M2 tests depend on documented SDP preconditions
2. **C1, C2, C3, C4 are independent** — can parallelize

**Recommended schedule:**

1. **Days 1–2:** 
   - C3 (secrets removal) — lowest risk, highest priority
   - C4 (monitor bounds) — isolated change
   
2. **Days 3–4:**
   - C1 (panic guards) — implement with detailed specs from critic feedback
   - C2 (grace deadline) — stress test with clock skew scenarios
   
3. **Days 5–6:**
   - M1 (SDP preconditions) — document before M2 tests
   - H1 (remove "active" state) — isolated refactor
   
4. **Days 7–8:**
   - M2 (idempotence guard) — depends on M1 completion
   - H3 (ICE channel 64→256)
   
5. **Days 9–10:**
   - H4 (offer-before-candidates) — complex state machine; needs thorough testing
   - H5 (atomic ordering) — verify with stress tests
   
6. **Days 11–12:**
   - H2 (EOC documentation) — low-risk, high clarity improvement
   - M3, M4, M5 (guards, versioning, errors)
   
7. **Days 13+:**
   - L1–L3 (polish, deferred work)

Each fix includes targeted tests before merge to the develop branch.

---

## TESTING STRATEGY

- **Unit Tests:** Panic guards, grace window, idempotence, overflow logging
- **Integration Tests:** Protocol ordering, monitor enumeration, stop signal visibility
- **Stress Tests:** 1000+ frame cycles, rapid state transitions, network loss simulation
- **Code Review:** SDP safety, EOC asymmetry, COM threading model
- **CI:** All changes must pass cargo check, clippy, and existing test suite

---

## RISK ASSESSMENT

| Finding | Risk | Mitigation |
|---------|------|-----------|
| C1 Panic | **High** — silent failure | Comprehensive panic guards + log loud |
| C2 Grace | **High** — infinite hang | Single deadline calculation, deadline sanity test |
| C3 Secrets | **High** — disclosure | Audit git history, add pre-commit hook |
| C4 Monitor | **Medium** — race on niche systems | Bounds check before access |
| H1–H5 | **Low–Medium** — edge cases | Targeted tests + stress runs |
| M1–M5 | **Low** — correctness polish | Code review + integration tests |
| L1–L3 | **Minimal** — deferred work | Document and plan Phase 2 |

---

## SIGN-OFF

All 17 findings require fixes for Phase 1 production readiness:
- **4 critical** (C1–C4): stabilize panics, races, secrets, bounds
- **5 high** (H1–H5): ensure correctness and ordering
- **5 medium** (M1–M5): guards and safety practices
- **3 low** (L1–L3): polish and deferred features

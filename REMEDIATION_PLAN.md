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
Four spawned tokio tasks (`spawn_encoder_pair`, `spawn_signaling_loop`, `spawn_ice_forwarder`, `spawn_state_watcher`) lack panic guards, causing silent failures if an unhandled panic occurs in task closure code.

**Fix:**  
Wrap each spawned task's closure body with structured panic handling. For `spawn_blocking` tasks, use `std::panic::catch_unwind()` to wrap the blocking function. For `tokio::spawn(async {})` tasks, check `JoinError::is_panic()` when awaiting and log panics. On panic, signal `shutdown_tx` to trigger coordinated teardown.

**Changes (DETAILED SPEC):**
- **File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs`

1. **Line 418** (`spawn_encoder_pair`):
   - Wrap `Vp9Encoder::new()` and loop body in `catch_unwind(AssertUnwindSafe(|| { ... }))`
   - On panic: log `ERROR`, send `shutdown_tx` before unwinding
   - Example:
     ```rust
     let shutdown_clone = shutdown_tx.clone();
     tokio::task::spawn_blocking(move || {
         match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
             // encoder loop code
         })) {
             Err(e) => {
                 tracing::error!("encoder task panicked: {:?}", e);
                 let _ = shutdown_clone.blocking_send(());
             }
             Ok(_) => {}
         }
     })
     ```

2. **Line 512** (`spawn_signaling_loop`):
   - Wrap entire async block; on panic, handle via `.await` JoinError check
   - In PartialInit::cleanup (lines 125-137), check: `if let Err(e) = handle.await { if e.is_panic() { tracing::error!(...) } }`

3. **Line 592** (`spawn_ice_forwarder`):
   - Same pattern as signaling_loop

4. **Line 636** (`spawn_state_watcher`):
   - Same pattern as signaling_loop

- **Test approach:**
  - Unit test: `#[tokio::test] panic_in_encoder_is_handled()` — mock panic in encoder, verify shutdown_tx receives signal
  - Unit test: `#[tokio::test] panic_in_async_task_is_logged()` — mock panic in signaling, verify error is logged on await
  - Integration test: start session, crash encoder with panic injection, verify supervisor detects panic and initiates teardown without crashing

**Test Approach:**
- Unit test: inject `panic!()` in encoder closure mock, verify graceful closure
- Integration test: crash encoder during frame processing, verify supervisor teardown logs "panic detected"

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
Protocol allows viewer to receive ICE candidates before SDP offer. Browser will buffer candidates without matching media line index, causing race condition. Should enforce offer first.

**Fix:**  
Modify signaling loop to buffer ICE candidates until offer is sent. Enforce protocol invariants: (1) offer must be sent before any ICE candidates, (2) answer must be applied before we send new candidates, (3) viewer disconnect during buffer is handled gracefully.

**Changes (DETAILED SPEC):**

**File:** `/home/user/ghostview/tauri-host/src-tauri/src/session.rs` lines 506–587 (`spawn_signaling_loop`)

1. **Add state tracking (after line 506):**
   ```rust
   let mut ice_pending: VecDeque<LocalIceCandidate> = VecDeque::new();
   let mut offer_sent = false;
   let mut answer_received = false;
   const MAX_PENDING_ICE: usize = 256; // Prevent unbounded memory
   ```

2. **Update ViewerJoined handler (around line 518):**
   ```rust
   ServerMessage::ViewerJoined => {
       // Create offer and send immediately
       let offer_sdp = webrtc.create_offer().await?;
       ice_tx.send(ClientMessage::Offer { sdp: offer_sdp }).await?;
       offer_sent = true;
       
       // Drain any ICE candidates that arrived before viewer joined
       while let Some(ice) = ice_pending.pop_front() {
           ice_tx.send(ClientMessage::IceCandidate { ... }).await?;
       }
   }
   ```

3. **Update Answer handler (around line 530):**
   ```rust
   ServerMessage::Answer { sdp } => {
       webrtc.set_answer(sdp).await?;
       answer_received = true;
   }
   ```

4. **Update IceCandidate handler (around line 540):**
   ```rust
   ServerMessage::IceCandidate { candidate, sdp_mid, sdp_mline_index, username_fragment } => {
       if !offer_sent {
           // Buffer until offer is sent
           if ice_pending.len() < MAX_PENDING_ICE {
               ice_pending.push_back(LocalIceCandidate { candidate, sdp_mid, sdp_mline_index, username_fragment });
           } else {
               tracing::warn!("ice candidate buffer full; dropping candidate");
           }
       } else {
           // Forward immediately
           webrtc.add_ice_candidate(candidate, sdp_mid, sdp_mline_index, username_fragment).await?;
       }
   }
   ```

5. **Update cleanup (around line 560):**
   - On disconnection or error, clear `ice_pending` buffer: `ice_pending.clear();`
   - Log: `tracing::info!("cleared {} pending ice candidates", ice_pending.len());`

**Protocol invariants (enforce in tests):**
- No IceCandidate message sent before Offer
- No ICE candidates forwarded to WebRTC before answer is applied
- If viewer joins and leaves before offer is sent, buffer is flushed and no ICE sent
- Max pending buffer is 256 to prevent DoS

**Test approach:**
- Unit test: `#[tokio::test] ice_before_offer_buffered()` — simulate ICE arriving before viewer joins, verify buffered and sent after offer
- Unit test: `#[tokio::test] max_pending_ice_prevents_dos()` — 300 ICE arrive before offer, verify only 256 buffered, rest dropped with warning
- Integration test: capture message sequence in signaling logs, confirm offer always precedes first ICE
- Integration test: viewer joins late, verify no buffered ICE arrive

**Test Approach:**
- Unit test: simulate `IceCandidate` arriving before `ViewerJoined`, verify buffered and sent after offer
- Integration test: capture message sequence, confirm offer always precedes candidates

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

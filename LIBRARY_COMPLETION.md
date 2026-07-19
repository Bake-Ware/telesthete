# Telesthete library — completion plan & verified status

Goal: bring **both** reference libraries (Python `telesthete/`, Rust
`rust/telesthete/`) to the same bar as the reference hub (`telesthitium`):
complete against `SPEC.md`, TDD, cross-language conformance vectors byte-identical,
adversarially reviewed. `SPEC.md` is a living contract and is corrected where the
audit found it wrong/underspecified.

This supersedes the optimistic status in `PROTOCOL_STATUS.md`. It records the
verified audit (2026-07, per-spec-section, both languages, code-checked) and the
phased roadmap.

## Locked decisions
- **Nonce-reuse fix:** one monotonic sequence counter per sender per band
  (shared across Control/Stream/Channel), **CSPRNG-initialized** (63-bit start,
  headroom against wrap). Generalizes rook's build-31 fix. Wire-compatible
  (sequence is already an 8-byte cleartext, "monotonic per-sender"). `SPEC.md`
  §3.3 gains the MUST-random-init mandate + the accept-first-then-strict
  watermark rule.
- **SPEC.md is authoritative + correctable:** where the two impls diverge, the
  spec is made canonical and both conform; corrections are summarized per phase.

## Verified status (audit, code-checked — NOT the self-assessment)

| Area (SPEC) | Python | Rust | Headline defects |
|---|---|---|---|
| Crypto primitives §3.1/§3.2 | ✅ | ✅ | byte-identical, vector-pinned |
| **AEAD nonce uniqueness §3.3** | ❌ critical | ❌ critical | **cross-sender + cross-channel + restart nonce reuse** (band-wide key, seq-only nonce, counters start 0/1) |
| Cipher negotiation data path §3.5 | ✅ | ❌ | Rust never applies the negotiated suite (baseline only) |
| Replay watermark §3.3 | ◑ | ◑ | Rust drops seq≤0 (kills Python's first frame); both restart-lockout; unauth watermark growth |
| Control §4 | ◑ | ◑ | no KEYFRAME_REQ/RATE_HINT; unknown-type not MUST-ignored; no keepalive scheduling (Rust) |
| Stream §5 | ◑ | ◑ | Python inserts non-spec timestamp (breaks interop); no StreamHeader/dmabuf (Py); no §5.3 priority; Rust dmabuf int-truncation, per-peer demux bug |
| Channel §6 (reliable) | ◑ stub | ◑ stub | retransmit no-op + ACK off-by-one + no replay (Py); no reliability layer at all (Rust); fragmentation unused |
| Board §7 | ⬜ | ⬜ | unimplemented (spec thin — design needed) |
| Drop §8 | ⬜ | ⬜ | unimplemented (spec thin — design needed) |
| UDP §9.1 | ◑ | ◑ | no §9.1 send-priority (both); unbounded send queue (Py); no shutdown/backoff (Rust) |
| LAN discovery §9.2 | ◑ wrong | ⬜ | Python wire format wholly non-conformant (10-byte magic, ver 1, len-prefixed host); Rust missing |
| AF_UNIX §9.4 | ⬜ | ◑ | Python missing; **Rust is DGRAM not SEQPACKET → cannot reach the hub's socket**; no replay; truncation |
| WSS §9.3 | ⬜ | ⬜ | client transport missing both |
| WebTransport §9.6 | ⬜ | ⬜ | client transport missing both |
| Framing/constants §1/§12.4 | ◑ | ✅ | Python accepts <43B and unknown types; LOCAL_PSK missing (Py); stale "XChaCha" docs (Rust) |

**Cross-language interop is currently broken** (Stream timestamp, discovery
format, seq=0 drop, divergent Channel wire) — a Python peer and a Rust peer
cannot talk today. There is also no live Python↔Rust socket interop test.

## Progress (updated as phases land on `lib/full-implementation`)

Every defect in the table above is now fixed except the deliberately
re-scoped items below. Phases 1–8 are complete in BOTH languages: shared
CSPRNG SequenceSource + session-epoch re-key/rebase (1), framing/wire
conformance + vectors (2), Rust negotiated-cipher data path + keepalive/
dead-peer/auto-ACK Band driving (3), KEYFRAME_REQ/RATE_HINT + unknown-type
MUST-ignore (4), StreamHeader/dmabuf in Python + dmabuf bounds hardening +
timestamp removal → live Python↔Rust interop (5), full reliable Channel
with the §6.1 inner-seq amendment (6), §9.2-conformant discovery ×2 +
Python AF_UNIX SEQPACKET (7), Board §7 + Drop §8 specified and implemented
(8). Epoch-downgrade replay guard added both languages (found in phase 3).

Re-scoped, with rationale recorded in SPEC/commits:
- **§5.3/§9.5 send priority:** both references send synchronously (no
  queue), so there is nothing to reorder; priority is carried per §5.1.
- **WSS/WebTransport clients:** §9.3 remains Future; §12.2 scopes these to
  browser/Kotlin/ESP32 clients, not the reference libraries. The hub
  serves both; the references speak UDP + AF_UNIX.
- **Rust AF_UNIX keeps SOCK_DGRAM for peer-to-peer** (spec-permitted;
  `telesthete-c` + consumer examples depend on the connectionless model)
  and **adds `UnixSeqClient`**, a SOCK_SEQPACKET client — the hub's
  listener is SEQPACKET-only, so this closes the audit's "cannot reach
  the hub's socket" defect. Python demonstrates a SEQPACKET
  server+client transport.

## Phase roadmap (each phase: both langs + spec + vectors + TDD, then report)

1. **Crypto/sequence/watermark core** — shared CSPRNG sequence source; accept-first
   watermark; HELLO `session_id` for restart-safe watermark reset (SPEC §3.3/§4.3);
   select_cipher baseline validation. Fixes the critical nonce reuse + the seq=0
   interop drop + restart lockout.
2. **Framing/wire conformance** — enforce MIN_PACKET_SIZE=43 (Py); define
   unknown-channel-type handling (SPEC §2/§4.2); LOCAL_PSK (Py); fix stale Rust
   docs; remove orphaned Rust test files; cross-impl framing vectors.
3. **Cipher-negotiation data path (Rust)** — apply committed suite per peer;
   baseline-only-for-HELLO; negotiation vectors.
4. **Control completeness** — KEYFRAME_REQ/RATE_HINT (§4.9/4.10); keepalive
   scheduling + 15s dead-peer detection; §4.2 MUST-ignore unknown types.
5. **Stream completeness** — remove Python timestamp; StreamHeader+dmabuf (Py);
   §5.2 watermark drop; §5.3 multiplexing priority; per-peer demux + dmabuf bounds
   (Rust); capability-gated flag rejection.
6. **Channel §6 real reliability** — handshake (§6.3), retransmit/window (§6.4),
   states (§6.5), fragmentation integration (§6.6), replay protection, ACK fix.
   (Largest phase — a full reliable transport ×2.)
7. **Transports** — §9.1 send-priority; LAN discovery (Rust) + fix Python wire;
   AF_UNIX Python (SEQPACKET+SCM_RIGHTS) + Rust DGRAM→SEQPACKET; WSS client (both);
   WebTransport client (both).
8. **Board §7 + Drop §8** — design conformant semantics, spec them, implement both,
   TDD.
9. **Interop + review** — live Python↔Rust socket harness; regenerate all vectors;
   adversarial review (independent reviewers, like the hub); PR.

## Phase 9 — adversarial review outcome

Ran a 6-lens multi-agent review (crypto/replay, Channel state machines,
Board/Drop, cross-language wire, Rust concurrency, Python robustness),
each finding attacked by two refute-by-default skeptics. Every confirmed
high/medium defect is fixed on this branch (two commits, `review-fix
(Python)` and `review-fix (Rust)`):

- **Channel §6:** RST/teardown stopped retransmitting forever + leaking
  the task (both); MAX_RETRIES cap for a vanished peer; reorder buffer
  and ack_num bounded; Rust gained the §3.3 outer-sequence replay
  watermark it lacked; Channel is now reliable-only and always
  §6.6-enveloped (removed Rust's forgeable headerless `ChannelSender`
  and Python's raw non-enveloped path — they broke Python↔Rust interop
  and let user bytes forge ACK/window).
- **Task/socket leak (Rust):** every hub + ControlChannel now aborts its
  task on drop, so a dropped Band actually releases the UDP socket.
- **Control/HELLO:** stale-epoch HELLO can no longer ratchet the control
  watermark and wedge the plane (Python); unknown committed cipher →
  baseline; absent session → 0 not -1.
- **Drop §8:** OFFER validated; resumed chunks pruned to in-range/correct
  length (no `_finish` crash). **Board §7:** out-of-range Lamport rejected
  (digest DoS). **Reassembler:** evicts oldest, never the in-flight msg.
- **Bounded growth:** discovery seen-set, control replay maps, and the
  peer registry are all capped (spoofed-source-HELLO defense).

Full return-routability for the spoofed-source peer-creation vector is a
protocol change (challenge in the handshake) left for a future revision;
the caps bound worst-case memory in the meantime.

Not merged until the owner approves. The hub PR (#7) is already merged on `main`.

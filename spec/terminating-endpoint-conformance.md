# Terminating endpoint conformance

What an endpoint that holds many keys and terminates their bands (SPEC §3.6,
§10.2) claims, so that an application writing its own endpoint has the list.
The claims are tested against the reference `Endpoint` in path mode, and in
UDP mode where a test name says so. The WebSocket claims (T10, and T12 over
WebSocket) are tested against a minimal server built on the path seam in the
tests, not against the relay. The design behind the list is
`spec/terminating-endpoints.md` §5.6.

| ID | Claim | Python test |
|---|---|---|
| T1 | Frames under 43 bytes, and frames whose `band_id` has no key, are dropped without a decrypt call. | `tests/test_endpoint.py::test_t1_short_and_unknown_band_id_dropped_without_decrypt`<br>`tests/test_endpoint.py::test_t1_udp_mode` |
| T2 | No frame is sent in answer to a frame that fails authentication. | `tests/test_endpoint.py::test_t2_no_answer_to_a_frame_that_fails_authentication` |
| T3 | Keys are isolated: a peer reaches only its own Band; a frame under key A arriving on key B's path is handled under key A. | `tests/test_endpoint.py::test_t3_keys_are_isolated` |
| T4 | After `remove_key` returns, no frame for that `band_id` is decrypted or delivered. | `tests/test_endpoint.py::test_t4_remove_key_mid_message_then_nothing_decrypted` |
| T5 | Answers go out on the path of the key's latest accepted frame (UDP mode: from the address and port the peer sent to). | `tests/test_endpoint.py::test_t5_answers_follow_the_latest_accepted_frame`<br>`tests/test_endpoint.py::test_t5_udp_mode_answers_from_the_address_the_peer_sent_to` |
| T6 | A frame that fails authentication, a replayed frame, or an older-epoch HELLO does not move the path (while the endpoint holds the key's session state; see T15 and the non-claim below). | `tests/test_endpoint.py::test_t6_unauthenticated_replayed_or_older_epoch_frames_do_not_move_the_path` |
| T7 | A peer that reconnects on a new path, or changes UDP source port, keeps its session and its open Channels. | `tests/test_endpoint.py::test_t7_new_path_keeps_session_and_channels`<br>`tests/test_endpoint.py::test_t7_udp_source_port_change_keeps_session_and_channels` |
| T8 | `close_path` ends the path; the peer's next accepted frame on a new path resumes delivery. | `tests/test_endpoint.py::test_t8_close_path_ends_the_path_until_the_next_accepted_frame` |
| T9 | A blocked `send` for one path does not delay frames on other paths (the callback contract; checked with a test sender). | `tests/test_endpoint.py::test_t9_a_blocked_path_does_not_delay_the_others` |
| T10 | WebSocket (test server built on the seam, Python and Rust): upgrade on `/band` with a query string, no subprotocol; text and short binaries ignored; nothing sent before the first frame; pings answered; closed after 60 s silent. | `tests/test_endpoint_websocket.py::test_t10_websocket_server_built_on_the_seam` |
| T11 | Route (0x05) frames are never delivered to the application. | `tests/test_endpoint.py::test_t11_route_frames_never_reach_the_application` |
| T12 | After the endpoint restarts with a newer epoch, a peer with §4.11 and §6.7 recovers: over WebSocket on reconnect, over UDP within `PEER_TIMEOUT`, or at once after `connect`. | `tests/test_endpoint_websocket.py::test_t12_ws_peer_recovers_after_endpoint_restart`<br>`tests/test_endpoint.py::test_t12_udp_peer_recovers_within_peer_timeout`<br>`tests/test_endpoint.py::test_t12_udp_peer_recovers_at_once_after_connect` |
| T13 | Keys and plaintext do not appear in logs at default levels. | `tests/test_endpoint.py::test_t13_no_keys_or_plaintext_in_logs` |
| T14 | `stop` returns promptly with peers connected. | `tests/test_endpoint.py::test_t14_stop_returns_promptly_with_peers_connected` |
| T15 | While the endpoint holds a key, a recorded session of that key replayed on any path is not delivered and does not move the path, including after the peer sent GOODBYE or was evicted. | `tests/test_endpoint.py::test_t15_replay_rejected_while_the_key_is_held` |

T10 is tested with the server's idle timeout set to 1 s instead of 60 s, so
that the test runs quickly. T15 covers three cases: a replay while the peer
is connected, after it sent GOODBYE, and after it was evicted under §4.5.

**Stated non-claim.** A terminating endpoint does *not* protect itself
against a replay after its own restart. It also does nothing for a peer that
restarts: that peer accepts a replay of what the endpoint sent it before. The
list says so, so that no one reads T15 as more than it is.

The non-claim is pinned by two tests:

- `tests/test_endpoint.py::test_nonclaim_endpoint_without_the_key_state_accepts_a_replay`
  replays a recorded session to a restarted endpoint, and to one that
  removed and re-added the key;
- `tests/test_endpoint.py::test_nonclaim_restarted_peer_accepts_a_replay_of_what_the_endpoint_sent`
  replays to a restarted peer.

If the behaviour changes, one of them fails and this file needs updating.

The Rust `Endpoint` (a later pull request) adds a Rust column.

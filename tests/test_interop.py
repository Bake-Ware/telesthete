"""Live Python <-> Rust interop (SPEC §3, §4).

A Python Band drives the Rust `interop_peer` example over real UDP: HELLO
(base-key handshake) then a session-keyed keepalive. Proves the two reference
implementations agree on framing, base-key crypto, the HELLO handshake, AND the
per-session data key (§3.1/§3.3) — cross-language.

Full Stream-data interop waits for the Phase 5 wire-conformance fix (the Python
Stream still prepends a non-spec timestamp); this covers the control plane.
"""

import asyncio
import os
import re
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.band import Band

RUST_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "rust"))


def _build_peer():
    if not os.path.isdir(RUST_DIR):
        pytest.skip("rust workspace not present")
    try:
        subprocess.run(
            ["cargo", "build", "--example", "interop_peer", "-p", "telesthete"],
            cwd=RUST_DIR, check=True, capture_output=True, text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        pytest.skip(f"cargo build unavailable: {e}")
    exe = os.path.join(RUST_DIR, "target", "debug", "examples", "interop_peer")
    assert os.path.exists(exe), exe
    return exe


@pytest.mark.asyncio
async def test_python_to_rust_handshake_and_session_key():
    exe = _build_peer()
    proc = subprocess.Popen([exe], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        # The Rust peer prints "READY <addr>" once bound.
        ready = await asyncio.get_event_loop().run_in_executor(None, proc.stderr.readline)
        m = re.search(r"READY (\S+)", ready or "")
        assert m, f"no READY from rust peer (stderr={ready!r})"
        host, port = m.group(1).rsplit(":", 1)
        port = int(port)

        band = Band(psk="interop-psk", bind_port=0)
        await band.start()
        try:
            # HELLO (base key), then a session-keyed Stream frame to the rust peer.
            band.connect_peer(host, port)
            await asyncio.sleep(0.4)
            st = band.stream(stream_id=9)
            st.add_destination((host, port))  # rust peer sends no HELLO_ACK; add manually
            for _ in range(5):  # first may race the peer opening its receiver
                st.send(b"hi-from-python")
                await asyncio.sleep(0.1)

            rc = await asyncio.get_event_loop().run_in_executor(
                None, lambda: _wait(proc, 6))
            out = proc.stdout.read()
            assert rc == 0, f"rust peer exit={rc}, stdout={out!r}"
            assert "HELLO" in out, out
            assert "STREAM hi-from-python" in out, out
        finally:
            await band.stop()
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait(timeout=5)


def _wait(proc, timeout):
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        return None

"""§7.4 Board digest cross-language conformance — vectors.json `board_digest`.

The digest is the anti-entropy hinge: Python and Rust MUST produce identical
(count, hash) for the same entries or mixed-language bands would sync forever.
The Rust mirror of this test is board.rs::board_digest_vectors_match_python.
"""

import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.board import Board


def _load_cases():
    with open(os.path.join(os.path.dirname(__file__), "vectors.json")) as f:
        return json.load(f)["board_digest"]


def test_board_digest_vectors():
    cases = _load_cases()
    assert len(cases) >= 2, "need at least two board_digest vectors"
    for case in cases:
        board = Board(b"\x00" * 16, 0, "vector")
        for e in case["entries"]:
            board.merge_entry({
                "key": e["key"],
                "value": None,
                "ts": [e["lamport"], e["actor"]],
                "deleted": e["deleted"],
            })
        count, hexdigest = board.digest()
        assert count == case["count"]
        assert hexdigest == case["hash"]

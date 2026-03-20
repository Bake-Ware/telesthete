"""
Peer tracking and state management
"""

import time
import logging
from typing import Optional, Dict, Any

logger = logging.getLogger(__name__)


class Peer:
    """
    Represents a remote peer in a Band
    """

    def __init__(self, address: tuple, hostname: Optional[str] = None):
        """
        Initialize peer

        Args:
            address: (host, port) tuple
            hostname: Peer's hostname (if known)
        """
        self.address = address
        self.hostname = hostname or f"{address[0]}:{address[1]}"

        # Connection state
        self.connected_at = time.time()
        self.last_seen = time.time()

        # Keepalive tracking
        self.keepalive_timeout = 15.0  # seconds
        self.missed_keepalives = 0

        # Peer capabilities/settings
        self.settings: Dict[str, Any] = {}

        # Focus state
        self.has_focus = False

    def update_last_seen(self):
        """Update last seen timestamp"""
        self.last_seen = time.time()
        self.missed_keepalives = 0

    def is_alive(self) -> bool:
        """Check if peer is still alive based on last seen"""
        return (time.time() - self.last_seen) < self.keepalive_timeout

    def mark_keepalive_missed(self):
        """Mark a keepalive as missed"""
        self.missed_keepalives += 1

    def update_settings(self, settings: Dict[str, Any]):
        """
        Update peer settings

        Args:
            settings: Settings dictionary
        """
        self.settings.update(settings)

    def __repr__(self):
        return f"Peer({self.hostname}, {self.address}, alive={self.is_alive()})"

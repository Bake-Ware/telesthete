"""
WebSocket client transport (SPEC §9.3)

Speaks to a server (a relay, §10, or a terminating endpoint, §10.2) over one
WebSocket connection: one Telesthete frame per binary message, one band per
connection. On any close or error it reconnects with exponential backoff and
jitter, and after each (re)connect it calls the Band's connected hook, which
sends HELLO, so the server learns the band from the connection's first frame.

Needs the optional ``websockets`` package: ``pip install telesthete[ws]``.
The rest of the package does not import this module.
"""

import asyncio
import logging
import random
from collections import defaultdict
from typing import Callable, Dict, Optional, Tuple
from urllib.parse import urlsplit

try:
    from websockets.asyncio.client import connect
except ImportError as e:  # pragma: no cover - exercised in a subprocess test
    raise ImportError(
        "WebSocketTransport needs the 'websockets' package: "
        "pip install telesthete[ws]") from e

from ..protocol.framing import MIN_PACKET_SIZE

logger = logging.getLogger(__name__)


class WebSocketTransport:
    """
    WebSocket client with the same handler interface as UDPTransport.

    There is one remote: the server. Every received frame is attributed to
    ``peer_address``, the ``(host, port)`` of the URL, and ``send`` sends to
    the server whatever destination it is given.

    ``on_connected`` is called with no arguments after each (re)connect; a
    Band constructed with this transport sets it to send HELLO.
    """

    def __init__(self, url: str, *, ssl=None, open_timeout: float = 10.0,
                 reconnect_min: float = 0.5, reconnect_max: float = 30.0):
        """
        Args:
            url: full ws:// or wss:// URL of the server, normally ending /band
            ssl: ssl.SSLContext for wss:// (None verifies against system roots)
            open_timeout: seconds allowed for each connection attempt
            reconnect_min: first reconnect delay, doubled after each failure
            reconnect_max: ceiling of the reconnect delay
        """
        parts = urlsplit(url)
        if parts.scheme not in ("ws", "wss") or not parts.hostname:
            raise ValueError(f"not a ws:// or wss:// URL: {url!r}")
        self.url = url
        self.ssl = ssl
        self.open_timeout = open_timeout
        self.reconnect_min = reconnect_min
        self.reconnect_max = reconnect_max

        self.peer_address: Tuple[str, int] = (
            parts.hostname, parts.port or (443 if parts.scheme == "wss" else 80))
        # Local (host, port) of the current connection; None while disconnected.
        self.local_address: Optional[Tuple[str, int]] = None
        self.on_connected: Optional[Callable[[], None]] = None

        # Handlers: channel_type -> list of callbacks(peer_addr, packet_bytes)
        self._handlers: Dict[int, list] = defaultdict(list)
        # Outbound queue of the current connection; None while disconnected,
        # so frames sent then are dropped, as UDP loss would drop them.
        self._send_queue: Optional[asyncio.Queue] = None

        # Set once the current connection delivers a frame; only then does
        # the reconnect delay go back to reconnect_min.
        self._delivered = False

        self._started = False
        self._running = False
        self._run_task: Optional[asyncio.Task] = None

    def start(self):
        """Nothing to bind; the connection is made by run()."""
        if self._started:
            raise RuntimeError("Transport already started")
        self._started = True

    async def run(self):
        """Connect, serve the connection, and reconnect until stopped."""
        if not self._started:
            raise RuntimeError("Transport not started. Call start() first.")
        self._running = True
        self._run_task = asyncio.current_task()
        delay = self.reconnect_min
        try:
            while self._running:
                self._delivered = False
                try:
                    async with connect(self.url, ssl=self.ssl,
                                       open_timeout=self.open_timeout,
                                       compression=None,
                                       ping_interval=None) as ws:
                        await self._serve(ws)
                    logger.info(f"WebSocket to {self.url} closed")
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    logger.info(f"WebSocket to {self.url}: {e!r}")
                if not self._running:
                    break
                # A connection that delivered a frame had worked: retry soon.
                # One that ended before delivering any (closed at once, or on
                # an oversized message) counts as a failure, so the delay
                # keeps doubling.
                if self._delivered:
                    delay = self.reconnect_min
                await asyncio.sleep(delay * random.uniform(0.8, 1.2))
                delay = min(delay * 2, self.reconnect_max)
        except asyncio.CancelledError:
            logger.info("WebSocket transport stopped")

    async def stop(self):
        """Close the connection and stop reconnecting."""
        self._running = False
        task, self._run_task = self._run_task, None
        if task is not None and task is not asyncio.current_task():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        logger.info("WebSocket transport stopped")

    def register_handler(self, channel_type: int, handler: Callable):
        """Register a handler(peer_addr, packet_bytes) for a channel type."""
        self._handlers[channel_type].append(handler)
        logger.debug(f"Registered handler for channel_type={channel_type}")

    def send(self, destination, packet_bytes: bytes):
        """Queue a frame for the server; dropped while disconnected."""
        queue = self._send_queue
        if queue is None:
            logger.debug("WebSocket not connected, dropping packet")
            return
        queue.put_nowait(packet_bytes)

    async def _serve(self, ws):
        queue: asyncio.Queue = asyncio.Queue()
        self._send_queue = queue
        self.local_address = tuple(ws.local_address[:2])
        logger.info(f"WebSocket connected to {self.url}")
        sender = asyncio.create_task(self._send_loop(ws, queue))
        receiver = asyncio.create_task(self._recv_loop(ws))
        try:
            if self.on_connected is not None:
                try:
                    self.on_connected()
                except Exception as e:
                    logger.error(f"on_connected hook error: {e}")
            # Either side ending (close or error) ends the connection.
            done, _ = await asyncio.wait({sender, receiver},
                                         return_when=asyncio.FIRST_COMPLETED)
            for task in done:
                if not task.cancelled() and task.exception() is not None:
                    logger.info(f"WebSocket connection ended: {task.exception()!r}")
        finally:
            self._send_queue = None
            self.local_address = None
            for task in (sender, receiver):
                task.cancel()
            await asyncio.gather(sender, receiver, return_exceptions=True)

    async def _send_loop(self, ws, queue: asyncio.Queue):
        while True:
            await ws.send(await queue.get())  # bytes -> one binary message

    async def _recv_loop(self, ws):
        async for message in ws:
            # Text messages and binaries shorter than a frame are ignored.
            if not isinstance(message, bytes) or len(message) < MIN_PACKET_SIZE:
                continue
            self._delivered = True
            channel_type = message[16]
            for handler in self._handlers.get(channel_type, []):
                try:
                    result = handler(self.peer_address, message)
                    if asyncio.iscoroutine(result):
                        await result
                except Exception as e:
                    logger.error(f"Handler error for channel_type={channel_type}: {e}")

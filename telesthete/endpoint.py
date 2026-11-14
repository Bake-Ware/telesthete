"""
Endpoint - many keys on one set of transports (SPEC §3.6, §10.2)

An Endpoint holds many PSKs. Each PSK is an ordinary band with its own
`band_id`, so the Endpoint picks the key for a frame by the cleartext
`band_id` before it decrypts anything. Each key is served by a normal `Band`
whose transport is a per-key view of the Endpoint, and each key has one
remote peer, which the Band sees at `Endpoint.PEER` whatever path its frames
arrive on.

The Endpoint either binds its own UDP socket or is fed by the application's
transports through the path seam: `receive(path, frame)` for each frame that
arrives, `close_path(path)` when a connection is gone, and the `send(path,
frame)` callback for each frame to go out. A *path* is an opaque, hashable
token the application chooses: for UDP the source address, for WebSocket the
connection.
"""

import asyncio
import logging
import time
from collections import defaultdict, deque
from typing import Callable, Dict, List, Optional

from .band import Band
from .protocol.crypto import derive_band_id
from .protocol.framing import ChannelType, MIN_PACKET_SIZE, unpack_packet

logger = logging.getLogger(__name__)

# Frames waiting in the Endpoint's receive queue; past this, receive() drops
# and counts them, as a full socket buffer would.
RECEIVE_QUEUE_FRAMES = 1024


class _KeyView:
    """One key's transport: what its Band sees instead of a socket.

    `start`, `run` and `stop` do nothing; the Endpoint owns the I/O. Frames
    the Band sends to `Endpoint.PEER` go to the key's current path; with no
    current path they are dropped, as UDP loss would drop them.
    """

    def __init__(self, endpoint: "Endpoint"):
        self._endpoint = endpoint
        self._handlers: Dict[int, list] = defaultdict(list)
        self.path = None          # the key's current path (SPEC §10.2 rule 4)
        self.removed = False
        # While a received frame is being handled, the Band's sends are held
        # here, so that answers to an accepted frame go out on its path.
        self._held: Optional[list] = None

    @property
    def local_address(self):
        return self._endpoint._local_address

    def start(self):
        pass

    async def run(self):
        pass

    async def stop(self):
        pass

    def register_handler(self, channel_type: int, handler: Callable):
        self._handlers[channel_type].append(handler)

    def send(self, destination, packet_bytes: bytes):
        if self.removed or destination != Endpoint.PEER:
            return
        if self._held is not None:
            self._held.append(packet_bytes)
        elif self.path is not None:
            self._endpoint._send_path(self.path, packet_bytes)


class _Key:
    def __init__(self, band: Band, view: _KeyView):
        self.band = band
        self.view = view
        self.start_task: Optional[asyncio.Task] = None


class _UDPProtocol(asyncio.DatagramProtocol):
    """UDP mode: the seam with a built-in socket."""

    def __init__(self, endpoint: "Endpoint"):
        self._endpoint = endpoint

    def datagram_received(self, data, addr):
        self._endpoint.receive(addr, data)


class Endpoint:
    """
    Many keys on one set of transports; one remote peer per key.

    Endpoint(hostname=None, *, bind_address="0.0.0.0", bind_port=9999,
             send=None, capabilities=None, ciphers=None, session_epoch=None)

    Without `send` the Endpoint binds UDP on bind_address:bind_port and a
    path is the peer's (host, port). With `send` it opens no socket: the
    application calls `receive(path, frame)` and `close_path(path)` and
    gets `send(path, frame)` back. `send` must not block and must not raise;
    a frame it cannot send at once it drops, as UDP would.

    All calls are made from the event loop's thread.
    """

    # The address at which each key's Band sees its one remote peer.
    PEER = ("peer", 0)

    def __init__(
        self,
        hostname: Optional[str] = None,
        *,
        bind_address: str = "0.0.0.0",
        bind_port: int = 9999,
        send: Optional[Callable[[object, bytes], None]] = None,
        capabilities: Optional[List[str]] = None,
        ciphers: Optional[List[str]] = None,
        session_epoch: Optional[int] = None,
    ):
        """
        Args:
            hostname: advertised in every key's HELLO (auto-detected if None)
            bind_address: UDP mode only: address to bind
            bind_port: UDP mode only: port to bind
            send: path mode: callback(path, frame) for each frame to send
            capabilities: passed to every Band (SPEC §12.5)
            ciphers: passed to every Band (SPEC §3.5)
            session_epoch: shared by every Band; MUST increase across
                restarts exactly as for a Band (SPEC §4.3). Default:
                milliseconds since the Unix epoch, taken once.
        """
        self.hostname = hostname
        self.bind_address = bind_address
        self.bind_port = bind_port
        self._send = send
        self.capabilities = capabilities
        self.ciphers = ciphers
        self.session_epoch = (int(session_epoch) if session_epoch is not None
                              else int(time.time() * 1000))

        # Frames dropped before decryption because they are shorter than a
        # frame or name a band_id the Endpoint holds no key for (SPEC §3.6).
        self.dropped_unknown = 0
        # Frames dropped by receive() because the receive queue was full.
        self.dropped_full = 0

        self._keys: Dict[bytes, _Key] = {}
        self._local_address = None
        self._udp: Optional[asyncio.DatagramTransport] = None

        # Receive queue: (path, frame) items, and (path, None) markers left
        # by close_path. Frames that arrived on a path before it was closed
        # are dropped when they come up (see close_path).
        self._queue: deque = deque()
        self._queued_frames = 0
        self._closing: Dict[object, int] = {}
        self._wakeup = asyncio.Event()

        self._running = False
        self._drain_task: Optional[asyncio.Task] = None

    # ---------------------------------------------------------------- keys

    def add_key(self, psk: str) -> Band:
        """Hold `psk`; return its Band (idempotent for the same psk).

        The Band's one peer is at `Endpoint.PEER`. It is created with
        reconnect=True (SPEC §4.11, §6.7) and, if the Endpoint is running,
        started at once."""
        band_id = derive_band_id(psk)
        key = self._keys.get(band_id)
        if key is not None:
            return key.band
        view = _KeyView(self)
        band = Band(psk, hostname=self.hostname,
                    capabilities=self.capabilities, ciphers=self.ciphers,
                    session_epoch=self.session_epoch, reconnect=True,
                    transport=view)
        key = _Key(band, view)
        self._keys[band_id] = key
        if self._running:
            key.start_task = asyncio.get_running_loop().create_task(band.start())
        logger.info(f"Endpoint: key added for band {band_id.hex()[:16]}...")
        return band

    def remove_key(self, band_id: bytes) -> bool:
        """Stop holding a key. When this returns, no frame bearing `band_id`
        is decrypted or delivered (SPEC §3.6 rule 2). The Band's tasks are
        stopped without sending GOODBYE. Returns False if the key was not
        held."""
        key = self._keys.pop(band_id, None)
        if key is None:
            return False
        key.view.removed = True
        if key.start_task is not None and not key.start_task.done():
            key.start_task.cancel()
        band = key.band
        band._running = False
        for channel in band.channels.values():
            if channel._retransmit_task is not None:
                channel._retransmit_task.cancel()
        for task in band._tasks:
            task.cancel()
        logger.info(f"Endpoint: key removed for band {band_id.hex()[:16]}...")
        return True

    def band(self, band_id: bytes) -> Optional[Band]:
        key = self._keys.get(band_id)
        return key.band if key is not None else None

    def band_ids(self) -> List[bytes]:
        return list(self._keys)

    def connect(self, band_id: bytes, path) -> None:
        """Make `path` the key's current path and start the SPEC §4.11 HELLO
        retry towards it. In UDP mode a path is a (host, port) tuple. Raises
        KeyError if the key is not held."""
        key = self._keys[band_id]
        key.view.path = path
        key.band.connect_peer(*self.PEER)

    # ---------------------------------------------------------------- seam

    def receive(self, path, frame: bytes) -> None:
        """A frame arrived on `path`. Synchronous: queues the frame and
        returns. Frames under 43 bytes or for a band_id with no key are
        dropped and counted in `dropped_unknown`; when the queue is full the
        frame is dropped and counted in `dropped_full`."""
        if len(frame) < MIN_PACKET_SIZE or bytes(frame[:16]) not in self._keys:
            self.dropped_unknown += 1
            return
        if self._queued_frames >= RECEIVE_QUEUE_FRAMES:
            self.dropped_full += 1
            return
        self._queue.append((path, bytes(frame)))
        self._queued_frames += 1
        self._wakeup.set()

    def close_path(self, path) -> None:
        """The connection behind `path` is gone. A key whose current path it
        was has no current path until its peer's next accepted frame. Frames
        from `path` still queued are dropped. The Band is not told."""
        for key in self._keys.values():
            if key.view.path == path:
                key.view.path = None
        self._closing[path] = self._closing.get(path, 0) + 1
        self._queue.append((path, None))
        self._wakeup.set()

    def _send_path(self, path, frame: bytes) -> None:
        if self._send is not None:
            try:
                self._send(path, frame)
            except Exception as e:
                logger.error(f"Endpoint send callback error: {e!r}")
        elif self._udp is not None:
            self._udp.sendto(frame, path)

    # ------------------------------------------------------------- receive

    async def _drain(self):
        while True:
            while not self._queue:
                self._wakeup.clear()
                await self._wakeup.wait()
            path, frame = self._queue.popleft()
            if frame is None:
                left = self._closing[path] - 1
                if left:
                    self._closing[path] = left
                else:
                    del self._closing[path]
                continue
            self._queued_frames -= 1
            if path in self._closing:
                continue  # arrived on a path that has since been closed
            try:
                await self._deliver(path, frame)
            except Exception as e:
                logger.error(f"Endpoint receive error: {e!r}")
            await asyncio.sleep(0)

    async def _deliver(self, path, frame: bytes):
        key = self._keys.get(frame[:16])
        if key is None:
            self.dropped_unknown += 1  # removed while queued
            return
        try:
            packet = unpack_packet(frame)
        except ValueError:
            return
        if packet.channel_type == ChannelType.ROUTE:
            return  # SPEC §10.2 rule 2: an endpoint drops Route frames
        view = key.view
        handlers = view._handlers.get(packet.channel_type)
        if not handlers:
            return
        before = self._watermark(key.band, packet)
        view._held = []
        try:
            for handler in list(handlers):
                try:
                    result = handler(self.PEER, frame)
                    if asyncio.iscoroutine(result):
                        await result
                except Exception as e:
                    logger.error(f"Handler error for channel_type="
                                 f"{packet.channel_type}: {e!r}")
            # Accepted (authenticated, not a §3.3 replay, not ignored under
            # §4.3): the receiving object advanced its replay watermark to
            # this frame's sequence. Only then does the path move.
            if (not view.removed and
                    self._watermark(key.band, packet) == packet.sequence != before):
                view.path = path
        finally:
            held, view._held = view._held, None
            if not view.removed and view.path is not None:
                for out in held:
                    self._send_path(view.path, out)

    def _watermark(self, band: Band, packet) -> Optional[int]:
        """The replay watermark (SPEC §3.3) of the object that receives this
        frame's channel_type and channel_id, for the key's one peer."""
        ct, cid = packet.channel_type, packet.channel_id
        if ct == ChannelType.CONTROL:
            holder = band.control
        elif ct == ChannelType.STREAM:
            holder = band.streams.get(cid)
        elif ct == ChannelType.CHANNEL:
            holder = band.channels.get(cid)
        elif ct == ChannelType.BOARD:
            holder = band.boards.get(cid)
        elif ct == ChannelType.DROP:
            holder = band.drops.get(cid)
        else:
            holder = None
        if holder is None:
            return None
        return holder._recv_watermark.get(self.PEER)

    # ----------------------------------------------------------- lifecycle

    async def start(self):
        """Bind UDP (without `send`), start receiving, start every Band."""
        if self._running:
            return
        if self._send is None:
            loop = asyncio.get_running_loop()
            self._udp, _ = await loop.create_datagram_endpoint(
                lambda: _UDPProtocol(self),
                local_addr=(self.bind_address, self.bind_port))
            self._local_address = self._udp.get_extra_info("sockname")[:2]
            logger.info(f"Endpoint UDP bound to {self._local_address}")
        self._running = True
        self._drain_task = asyncio.create_task(self._drain())
        for key in list(self._keys.values()):
            await key.band.start()

    async def stop(self):
        """Stop every Band (each sends GOODBYE, as Band.stop does), then
        stop receiving and close the UDP socket."""
        if not self._running:
            return
        self._running = False
        keys = list(self._keys.values())
        for key in keys:
            if key.start_task is not None and not key.start_task.done():
                key.start_task.cancel()
        await asyncio.gather(*(k.band.stop() for k in keys),
                             return_exceptions=True)
        if self._drain_task is not None:
            self._drain_task.cancel()
            await asyncio.gather(self._drain_task, return_exceptions=True)
            self._drain_task = None
        if self._udp is not None:
            self._udp.close()
            self._udp = None
        logger.info("Endpoint stopped")

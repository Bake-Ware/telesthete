"""
AF_UNIX transport (SPEC §9.4)

Same frame format and AEAD as UDP; SOCK_SEQPACKET so message boundaries
are preserved, ordering is guaranteed, and SCM_RIGHTS ancillary fds
(dmabuf planes + optional fence) can ride alongside a Stream packet.
"""

import array
import asyncio
import inspect
import logging
import os
import socket
from collections import defaultdict
from typing import Callable, Dict, Optional, Tuple

logger = logging.getLogger(__name__)

MAX_FDS_PER_PACKET = 5  # 4 planes + 1 sync_file fence (SPEC §9.4)

# recvmsg control buffer must hold MAX_FDS_PER_PACKET int fds (SPEC §9.4).
_CMSG_SPACE = socket.CMSG_SPACE(MAX_FDS_PER_PACKET * array.array("i").itemsize)


def default_socket_path(band_id: bytes) -> str:
    """SPEC §9.4: $XDG_RUNTIME_DIR/telesthete/<band_id_hex>.sock. Directory
    permissions are the primary access control (0700)."""
    runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
    d = os.path.join(runtime, "telesthete")
    os.makedirs(d, mode=0o700, exist_ok=True)
    return os.path.join(d, band_id.hex() + ".sock")


class UnixTransport:
    """
    SOCK_SEQPACKET transport: the server binds + accepts; clients connect.

    Handler interface matches UDPTransport (register_handler(channel_type,
    cb(peer_addr, packet_bytes))); a handler declaring a third parameter also
    receives the tuple of SCM_RIGHTS fds (empty when none arrived). Peer
    addresses are ("unix", connection_id) tuples — SEQPACKET peers have no
    (host, port), but Band code only needs a hashable identity.
    """

    def __init__(self, path: str, server: bool = True):
        self.path = path
        self.server = server
        self.socket: Optional[socket.socket] = None

        self._handlers: Dict[int, list] = defaultdict(list)
        # connection_id -> connected socket (server); client uses id 0.
        self._conns: Dict[int, socket.socket] = {}
        self._next_conn_id = 1
        self._running = False

    # -- lifecycle ----------------------------------------------------------

    def start(self):
        if self.socket:
            raise RuntimeError("Transport already started")
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        if self.server:
            try:
                os.unlink(self.path)
            except FileNotFoundError:
                pass
            self.socket.bind(self.path)
            self.socket.listen(16)
            logger.info(f"AF_UNIX transport listening on {self.path}")
        else:
            self.socket.connect(self.path)
            self._conns[0] = self.socket
            logger.info(f"AF_UNIX transport connected to {self.path}")
        self.socket.setblocking(False)

    async def run(self):
        if not self.socket:
            raise RuntimeError("Transport not started. Call start() first.")
        self._running = True
        loop = asyncio.get_event_loop()
        if self.server:
            loop.add_reader(self.socket.fileno(), self._on_acceptable)
        else:
            loop.add_reader(self.socket.fileno(), self._on_readable, 0, self.socket)
        # add_reader callbacks do the work; run() just parks until stop().
        self._stopped = asyncio.Event()
        await self._stopped.wait()

    async def stop(self):
        self._running = False
        loop = asyncio.get_event_loop()
        if self.socket:
            try:
                loop.remove_reader(self.socket.fileno())
            except (ValueError, OSError):
                pass
        for conn in list(self._conns.values()):
            try:
                loop.remove_reader(conn.fileno())
            except (ValueError, OSError):
                pass
            conn.close()
        self._conns.clear()
        if self.socket:
            self.socket.close()
            self.socket = None
        if self.server:
            try:
                os.unlink(self.path)
            except FileNotFoundError:
                pass
        if hasattr(self, "_stopped"):
            self._stopped.set()
        logger.info("AF_UNIX transport stopped")

    # -- I/O ----------------------------------------------------------------

    def register_handler(self, channel_type: int, handler: Callable):
        self._handlers[channel_type].append(handler)

    def send(self, destination: Tuple, packet_bytes: bytes, fds: Tuple[int, ...] = ()):
        """Send one packet (one SEQPACKET message == one Telesthete packet),
        optionally with SCM_RIGHTS fds (Stream dmabuf, SPEC §9.4)."""
        if len(fds) > MAX_FDS_PER_PACKET:
            raise ValueError(f"{len(fds)} fds exceeds MAX_FDS_PER_PACKET ({MAX_FDS_PER_PACKET})")
        conn = self._conns.get(destination[1] if isinstance(destination, tuple) else destination)
        if conn is None:
            logger.warning(f"AF_UNIX send: unknown destination {destination}")
            return
        ancdata = []
        if fds:
            ancdata = [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                        array.array("i", fds).tobytes())]
        try:
            conn.sendmsg([packet_bytes], ancdata)
        except OSError as e:
            logger.error(f"AF_UNIX send error to {destination}: {e}")

    def _on_acceptable(self):
        try:
            conn, _ = self.socket.accept()
        except OSError:
            return
        conn.setblocking(False)
        conn_id = self._next_conn_id
        self._next_conn_id += 1
        self._conns[conn_id] = conn
        asyncio.get_event_loop().add_reader(
            conn.fileno(), self._on_readable, conn_id, conn)
        logger.debug(f"AF_UNIX peer connected: id={conn_id}")

    def _on_readable(self, conn_id: int, conn: socket.socket):
        try:
            data, ancdata, _flags, _addr = conn.recvmsg(65535, _CMSG_SPACE)
        except (BlockingIOError, InterruptedError):
            return
        except OSError:
            data = b""
        if not data:
            # Peer closed. SEQPACKET delivers EOF as an empty read.
            try:
                asyncio.get_event_loop().remove_reader(conn.fileno())
            except (ValueError, OSError):
                pass
            conn.close()
            self._conns.pop(conn_id, None)
            logger.debug(f"AF_UNIX peer disconnected: id={conn_id}")
            return

        # Take ownership of any SCM_RIGHTS fds immediately: if the packet is
        # dropped below, they are closed here rather than leaked (SPEC §9.4).
        fds = []
        for level, ctype, cdata in ancdata:
            if level == socket.SOL_SOCKET and ctype == socket.SCM_RIGHTS:
                arr = array.array("i")
                arr.frombytes(cdata[:len(cdata) - (len(cdata) % arr.itemsize)])
                fds.extend(arr)
        peer_addr = ("unix", conn_id)

        if len(data) < 17:
            logger.warning(f"AF_UNIX undersized packet from {peer_addr}")
            self._close_fds(fds)
            return
        if len(fds) > MAX_FDS_PER_PACKET:
            logger.warning(f"AF_UNIX packet with {len(fds)} fds > max; dropping")
            self._close_fds(fds)
            return

        channel_type = data[16]
        handlers = self._handlers.get(channel_type, [])
        if not handlers:
            self._close_fds(fds)
            return
        delivered_fds = False
        for handler in handlers:
            try:
                if self._wants_fds(handler):
                    handler(peer_addr, data, tuple(fds))
                    delivered_fds = True
                else:
                    handler(peer_addr, data)
            except Exception as e:
                logger.error(f"AF_UNIX handler error for channel_type={channel_type}: {e}")
        if fds and not delivered_fds:
            # No handler took ownership -> close, don't leak (SPEC §9.4).
            self._close_fds(fds)

    @staticmethod
    def _wants_fds(handler: Callable) -> bool:
        try:
            return len(inspect.signature(handler).parameters) >= 3
        except (TypeError, ValueError):
            return False

    @staticmethod
    def _close_fds(fds):
        for fd in fds:
            try:
                os.close(fd)
            except OSError:
                pass

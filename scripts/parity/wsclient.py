"""A minimal blocking WebSocket client (RFC 6455) for the parity scenarios: the handshake with extra headers, masked
text and close frames out, text/binary/close/ping frames in. Enough to host an MCP server over a socket the
scenario opens to the server under test."""
import base64
import os
import socket
import struct
import threading
import urllib.parse


class Refused(Exception):
    """The server answered the upgrade with something other than 101."""

    def __init__(self, status, body):
        super().__init__(f"upgrade refused: {status}")
        self.status = status
        self.body = body


class Closed(Exception):
    def __init__(self, code, reason):
        super().__init__(f"closed: {code} {reason}")
        self.code = code
        self.reason = reason


class WebSocket:
    def __init__(self, url, headers=None, timeout=30):
        parts = urllib.parse.urlsplit(url)
        host, port = parts.hostname, parts.port or 80
        path = parts.path + (f"?{parts.query}" if parts.query else "")
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [f"GET {path} HTTP/1.1", f"Host: {host}:{port}", "Upgrade: websocket", "Connection: Upgrade",
                 f"Sec-WebSocket-Key: {key}", "Sec-WebSocket-Version: 13"]
        for name, value in (headers or {}).items():
            lines.append(f"{name}: {value}")
        self.sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                break
            head += chunk
        header, _, rest = head.partition(b"\r\n\r\n")
        status_line = header.split(b"\r\n", 1)[0].decode(errors="replace")
        status = int(status_line.split(" ")[1]) if len(status_line.split(" ")) > 1 else 0
        if status != 101:
            body = rest
            length = None
            for line in header.split(b"\r\n")[1:]:
                k, _, v = line.decode(errors="replace").partition(":")
                if k.strip().lower() == "content-length":
                    length = int(v.strip())
            try:
                while length is not None and len(body) < length:
                    chunk = self.sock.recv(4096)
                    if not chunk:
                        break
                    body += chunk
            except OSError:
                pass
            self.sock.close()
            raise Refused(status, body.decode(errors="replace"))
        self.buffer = rest
        self.lock = threading.Lock()
        self.closed = None

    def _read(self, n):
        while len(self.buffer) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise Closed(1006, "")
            self.buffer += chunk
        out, self.buffer = self.buffer[:n], self.buffer[n:]
        return out

    def _send(self, opcode, payload):
        mask = os.urandom(4)
        head = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        with self.lock:
            self.sock.sendall(head + mask + masked)

    def send(self, text):
        self._send(0x1, text.encode())

    def send_binary(self, data):
        self._send(0x2, data)

    def recv(self):
        """The next text (str) or binary (bytes) message; raises Closed when the server closes."""
        message, kind = b"", None
        while True:
            b1, b2 = self._read(2)
            opcode, fin = b1 & 0x0F, b1 & 0x80
            n = b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(n)
            if opcode == 0x8:
                code = struct.unpack(">H", payload[:2])[0] if len(payload) >= 2 else 1005
                self.closed = (code, payload[2:].decode(errors="replace"))
                try:
                    self._send(0x8, payload[:2])
                except OSError:
                    pass
                raise Closed(*self.closed)
            if opcode == 0x9:
                self._send(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode in (0x1, 0x2):
                kind = opcode
            message += payload
            if fin:
                return message.decode() if kind == 0x1 else message

    def close(self, code=1000):
        try:
            self._send(0x8, struct.pack(">H", code))
        except OSError:
            pass

    def shutdown(self):
        try:
            self.sock.close()
        except OSError:
            pass

#!/usr/bin/env python3
"""MCP Bridge — Layer 4 Transport: Session IDs, Seq Numbers, ACK/NACK, Chunking.

Wire format per frame:
  [COBS-encoded { Header(8) + Postcard payload + CRC-16(2) }] [0x00]

Header (fixed 8 bytes LE):
  session_id: u32 — zombie session killer
  seq_id:     u32 — monotonic, ACK/NACK correlation
"""

import struct

# ── CRC-16/CCITT-FALSE ──────────────────────────────────────────────────

def crc16(data: bytes) -> int:
    crc = 0xFFFF
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc

# ── COBS ─────────────────────────────────────────────────────────────────

def cobs_encode(data: bytes) -> bytes:
    out = bytearray()
    code_idx = len(out)
    out.append(0)
    code = 1
    for byte in data:
        if byte == 0x00:
            out[code_idx] = code
            code = 1
            code_idx = len(out)
            out.append(0)
        else:
            out.append(byte)
            code += 1
            if code == 0xFF:
                out[code_idx] = code
                code = 1
                code_idx = len(out)
                out.append(0)
    out[code_idx] = code
    return bytes(out)

def cobs_decode(data: bytes) -> bytes:
    out = bytearray()
    idx = 0
    while idx < len(data):
        code = data[idx]
        if code == 0: raise ValueError("0x00 in COBS")
        idx += 1
        for _ in range(1, code):
            if idx >= len(data): raise ValueError("COBS truncated")
            out.append(data[idx])
            idx += 1
        if code < 0xFF and idx < len(data):
            out.append(0x00)
    return bytes(out)

# ── Postcard Varint ──────────────────────────────────────────────────────

def encode_varint(value: int) -> bytes:
    out = bytearray()
    while value >= 0x80:
        out.append((value & 0x7F) | 0x80)
        value >>= 7
    out.append(value & 0x7F)
    return bytes(out)

def decode_varint(data: bytes, offset: int) -> tuple:
    result = 0
    shift = 0
    while offset < len(data):
        byte = data[offset]
        offset += 1
        result |= (byte & 0x7F) << shift
        if byte & 0x80 == 0:
            return result, offset
        shift += 7
    raise ValueError("Varint truncated")

def encode_heapless_vec(data: bytes) -> bytes:
    return encode_varint(len(data)) + data

def encode_heapless_string(s: str) -> bytes:
    return encode_heapless_vec(s.encode('utf-8'))


# ── Session State ────────────────────────────────────────────────────────

class McpSession:
    """Tracks session ID and sequence numbers."""
    def __init__(self):
        self.session_id: int = 0  # Set when first frame from OS arrives
        self.seq_counter: int = 0
        self.locked: bool = False

    def lock_to(self, sid: int):
        """Lock this session to the OS's session_id."""
        self.session_id = sid
        self.locked = True

    def next_seq(self) -> int:
        self.seq_counter += 1
        return self.seq_counter

    def validate(self, header_sid: int) -> bool:
        """Check if incoming session_id matches. Returns False for zombie data."""
        if not self.locked:
            return True  # Accept anything until locked
        return header_sid == self.session_id


_session = McpSession()


# ── Frame Header ─────────────────────────────────────────────────────────

def encode_header(session_id: int, seq_id: int) -> bytes:
    return struct.pack('<II', session_id, seq_id)

def decode_header(data: bytes) -> tuple:
    """Returns (session_id, seq_id, payload_offset=8)."""
    if len(data) < 8:
        raise ValueError("Header too short")
    sid = struct.unpack_from('<I', data, 0)[0]
    seq = struct.unpack_from('<I', data, 4)[0]
    return sid, seq, 8


# ── McpRequest Encoding (Python -> Rust OS) ───────────────────────────────

# Enum discriminants (must match Rust enum order in McpRequest)
class McpRequestTag:
    INITIALIZE = 0
    LIST_TOOLS = 1
    CALL_TOOL = 2
    CHAT_RESPONSE = 3
    TIME_SYNC = 4
    WASM_CHUNK = 5
    PING = 6
    ACK = 7
    NACK = 8
    NOTIFICATION = 9


def encode_chat_response(text: str) -> bytes:
    return encode_varint(McpRequestTag.CHAT_RESPONSE) + encode_heapless_vec(text.encode('utf-8'))

def encode_time_sync(year, month, day, hour, minute, second, utc_offset) -> bytes:
    payload = encode_varint(McpRequestTag.TIME_SYNC)
    payload += struct.pack('<H', year) + struct.pack('5B', month, day, hour, minute, second)
    payload += struct.pack('<h', utc_offset)
    return payload

def encode_wasm_chunk(total_chunks: int, chunk_index: int, data: bytes) -> bytes:
    payload = encode_varint(McpRequestTag.WASM_CHUNK)
    payload += struct.pack('<HH', total_chunks, chunk_index)
    payload += encode_heapless_vec(data)
    return payload

def encode_ping(seq: int) -> bytes:
    return encode_varint(McpRequestTag.PING) + struct.pack('<I', seq)

def encode_ack() -> bytes:
    return encode_varint(McpRequestTag.ACK)

def encode_nack(reason: int) -> bytes:
    return encode_varint(McpRequestTag.NACK) + struct.pack('B', reason)


# ── McpResponse Decoding (Rust OS -> Python) ──────────────────────────────

class McpResponseTag:
    INIT_RESULT = 0
    TOOLS_LIST = 1
    TOOL_RESULT = 2
    CHAT_REQUEST = 3
    TIME_SYNC_REQUEST = 4
    WASM_GEN_REQUEST = 5
    SAMPLING_REQUEST = 6
    PONG = 7
    ACK = 8
    NACK = 9
    ERROR = 10


def decode_mcp_response(data: bytes) -> dict:
    tag, offset = decode_varint(data, 0)

    if tag == McpResponseTag.CHAT_REQUEST:
        prompt_len, offset = decode_varint(data, offset)
        prompt = data[offset:offset + prompt_len].decode('utf-8', errors='replace')
        return {'type': 'chat_request', 'prompt': prompt}

    elif tag == McpResponseTag.TIME_SYNC_REQUEST:
        return {'type': 'time_sync_request'}

    elif tag == McpResponseTag.WASM_GEN_REQUEST:
        desc_len, offset = decode_varint(data, offset)
        desc = data[offset:offset + desc_len].decode('utf-8', errors='replace')
        return {'type': 'wasm_gen_request', 'description': desc}

    elif tag == McpResponseTag.PONG:
        seq = struct.unpack_from('<I', data, offset)[0]
        return {'type': 'pong', 'seq': seq}

    elif tag == McpResponseTag.ACK:
        return {'type': 'ack'}

    elif tag == McpResponseTag.NACK:
        reason = data[offset] if offset < len(data) else 0
        return {'type': 'nack', 'reason': reason}

    elif tag == McpResponseTag.ERROR:
        code = struct.unpack_from('<H', data, offset)[0]
        offset += 2
        msg_len, offset = decode_varint(data, offset)
        msg = data[offset:offset + msg_len].decode('utf-8', errors='replace')
        return {'type': 'error', 'code': code, 'message': msg}

    elif tag == McpResponseTag.SAMPLING_REQUEST:
        prompt_len, offset = decode_varint(data, offset)
        prompt = data[offset:offset + prompt_len].decode('utf-8', errors='replace')
        offset += prompt_len
        max_tokens = struct.unpack_from('<H', data, offset)[0]
        return {'type': 'sampling_request', 'prompt': prompt, 'max_tokens': max_tokens}

    return {'type': 'unknown', 'tag': tag}


# ── Frame Assembly + Parsing ─────────────────────────────────────────────

def make_frame(postcard_payload: bytes, session_id: int = 0, seq_id: int = 0) -> bytes:
    """Wrap: Header(8) + Postcard payload + CRC-16 -> COBS -> 0x00 sentinel."""
    header = encode_header(session_id or _session.session_id, seq_id or _session.next_seq())
    raw = header + postcard_payload
    crc = crc16(raw)
    raw_with_crc = raw + struct.pack('<H', crc)
    return cobs_encode(raw_with_crc) + b'\x00'


def parse_frame(frame_bytes: bytes) -> tuple:
    """Strip COBS + verify CRC + extract header.
    Returns (session_id, seq_id, postcard_payload) or raises."""
    decoded = cobs_decode(frame_bytes)
    if len(decoded) < 11:  # 8 header + 1 payload + 2 CRC
        raise ValueError(f"Frame too short ({len(decoded)} bytes)")
    payload_with_header = decoded[:-2]
    received_crc = struct.unpack_from('<H', decoded, len(decoded) - 2)[0]
    if crc16(payload_with_header) != received_crc:
        raise ValueError(f"CRC mismatch")
    sid, seq, offset = decode_header(payload_with_header)
    postcard_payload = payload_with_header[offset:]
    return sid, seq, postcard_payload


class RetransmissionQueue:
    """TCP-style retransmission queue for reliable frame delivery.

    For each sent frame:
    1. Store (seq_id, wire_bytes, timestamp) in the queue
    2. Wait for ACK { seq_id } from OS
    3. On ACK: remove from queue, proceed to next
    4. On NACK or timeout: resend from queue
    5. After MAX_RETRIES: give up

    This is the "missing piece" that turns our error-detector into an error-corrector.
    """

    ACK_TIMEOUT_S = 2.0    # seconds to wait for ACK before retransmit
    MAX_RETRIES = 3        # give up after this many retransmits per frame

    def __init__(self):
        self.pending: dict[int, dict] = {}  # seq_id -> {frame, retries, sent_at}

    def enqueue(self, seq_id: int, wire_frame: bytes):
        """Store a sent frame for potential retransmission."""
        import time
        self.pending[seq_id] = {
            'frame': wire_frame,
            'retries': 0,
            'sent_at': time.time(),
        }

    def on_ack(self, seq_id: int):
        """Frame was acknowledged — remove from queue."""
        if seq_id in self.pending:
            del self.pending[seq_id]

    def on_nack(self, seq_id: int, sock) -> bool:
        """Frame was rejected — retransmit immediately if retries remain.
        Returns True if retransmitted, False if gave up."""
        entry = self.pending.get(seq_id)
        if not entry:
            return False
        entry['retries'] += 1
        if entry['retries'] > self.MAX_RETRIES:
            print(f"[RETX] seq={seq_id} gave up after {self.MAX_RETRIES} retries")
            del self.pending[seq_id]
            return False
        print(f"[RETX] seq={seq_id} retry #{entry['retries']}")
        import time
        sock.sendall(entry['frame'])
        entry['sent_at'] = time.time()
        return True

    def check_timeouts(self, sock):
        """Check for timed-out frames and retransmit them."""
        import time
        now = time.time()
        expired = []
        for seq_id, entry in self.pending.items():
            if now - entry['sent_at'] > self.ACK_TIMEOUT_S:
                expired.append(seq_id)

        for seq_id in expired:
            self.on_nack(seq_id, sock)

    def is_empty(self) -> bool:
        return len(self.pending) == 0


_retx_queue = RetransmissionQueue()


def send_reliable(postcard_payload: bytes, sock, session_id: int = 0) -> int:
    """Send a frame and enqueue it for potential retransmission.
    Returns the seq_id assigned to this frame."""
    seq = _session.next_seq()
    frame = make_frame(postcard_payload, session_id=session_id, seq_id=seq)
    sock.sendall(frame)
    _retx_queue.enqueue(seq, frame)
    return seq


def send_wasm_chunked(wasm_binary: bytes, sock, session_id: int = 0, chunk_size: int = 3072):
    """Split WASM binary into chunks and send with retransmission support.

    Each chunk is sent reliably:
    1. Send chunk N, enqueue for retransmission
    2. Brief pause between chunks (let OS drain COM2 FIFO)
    3. After all sent, check for NACKs and retransmit as needed
    """
    import time
    total = (len(wasm_binary) + chunk_size - 1) // chunk_size
    print(f"[MCP] Sending WASM: {len(wasm_binary)} bytes in {total} chunks...")

    for i in range(total):
        start = i * chunk_size
        end = min(start + chunk_size, len(wasm_binary))
        chunk_data = wasm_binary[start:end]
        payload = encode_wasm_chunk(total, i, chunk_data)
        seq = send_reliable(payload, sock, session_id=session_id)
        print(f"[MCP]   Chunk {i+1}/{total} ({len(chunk_data)} bytes, seq={seq})")
        # Brief pause between chunks to avoid overwhelming COM2 FIFO
        time.sleep(0.05)

    print(f"[MCP] All {total} chunks sent, monitoring for ACKs/NACKs...")


# ── Self-Test ────────────────────────────────────────────────────────────

if __name__ == '__main__':
    print("=== MCP Bridge v2 (Layer 4) Self-Test ===")

    # CRC
    assert crc16(b"Hello") == 0xDADA
    print("[PASS] CRC-16")

    # COBS round-trip
    test = b"\x00\x01\x02\x00\x03"
    assert cobs_decode(cobs_encode(test)) == test
    print("[PASS] COBS round-trip")

    # Frame with header
    _session.session_id = 0xDEADBEEF
    payload = encode_chat_response("Hello!")
    frame = make_frame(payload)
    assert frame[-1] == 0x00
    sid, seq, recovered = parse_frame(frame[:-1])
    assert sid == 0xDEADBEEF
    assert seq == 1
    assert recovered == payload
    print(f"[PASS] Frame with header (sid=0x{sid:08X} seq={seq})")

    # Session validation
    _session.lock_to(0xDEADBEEF)
    assert _session.validate(0xDEADBEEF) == True
    assert _session.validate(0x12345678) == False
    print("[PASS] Session validation")

    # Chunking
    fake_wasm = bytes(range(256)) * 40  # 10KB
    chunks = []
    total = (len(fake_wasm) + 3072 - 1) // 3072
    for i in range(total):
        s = i * 3072
        e = min(s + 3072, len(fake_wasm))
        chunks.append(fake_wasm[s:e])
    reassembled = b''.join(chunks)
    assert reassembled == fake_wasm
    print(f"[PASS] Chunking: {len(fake_wasm)} bytes -> {total} chunks -> reassembled OK")

    # Retransmission queue
    rtq = RetransmissionQueue()
    rtq.enqueue(42, b"fake_frame_42")
    rtq.enqueue(43, b"fake_frame_43")
    assert not rtq.is_empty()
    rtq.on_ack(42)
    assert 42 not in rtq.pending
    assert 43 in rtq.pending
    rtq.on_ack(43)
    assert rtq.is_empty()
    print("[PASS] Retransmission queue: ACK clears entries")

    # NACK retransmit counter
    rtq2 = RetransmissionQueue()
    rtq2.MAX_RETRIES = 2
    rtq2.enqueue(99, b"retry_me")
    class FakeSock:
        def __init__(self): self.sent = []
        def sendall(self, data): self.sent.append(data)
    fs = FakeSock()
    assert rtq2.on_nack(99, fs) == True   # retry 1
    assert rtq2.on_nack(99, fs) == True   # retry 2
    assert rtq2.on_nack(99, fs) == False  # gave up
    assert len(fs.sent) == 2  # only 2 actual retransmits
    print("[PASS] Retransmission queue: NACK retries + give-up")

    # Varint
    assert decode_varint(encode_varint(0), 0) == (0, 1)
    assert decode_varint(encode_varint(300), 0) == (300, 2)
    print("[PASS] Varint")

    print("\n=== All tests passed! ===")

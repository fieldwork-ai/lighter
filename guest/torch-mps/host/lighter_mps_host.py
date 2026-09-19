"""The host half of `lighter.dev/mps`: PyTorch on the Mac's GPU, driven by a
container's PyTorch one operator at a time.

Runs in the user's own Python, whichever has `torch` with MPS. A container's
`lighter_mps` extension connects, checks versions, and then sends every
operator its tensors on `mps` are asked for; tensors live here, on `mps`,
and cross only as handles, except for the bytes of an upload or download.

Wire (little-endian): frames of `u32 kind, u64 len, payload`. Values are
tagged; see `encode`/`decode`. Every request has one reply of the same kind
or `ERR` with a message.
"""
import argparse
import socket
import struct
import sys
import threading
import traceback

import torch

HELLO, OP, UPLOAD, DOWNLOAD, FREE, ERR = 1, 2, 3, 4, 5, 0xFFFF

# c10::ScalarType by index; the guest sends these numbers.
DTYPES = [torch.uint8, torch.int8, torch.int16, torch.int32, torch.int64, torch.float16, torch.float32,
          torch.float64, torch.complex32, torch.complex64, torch.complex128, torch.bool, None, None, None,
          torch.bfloat16, None, None, None, None, None, None, None, None, None, None, None, None,
          torch.uint16, torch.uint32, torch.uint64]
DTYPE_INDEX = {d: i for i, d in enumerate(DTYPES) if d is not None}
MEMORY_FORMATS = [torch.contiguous_format, torch.preserve_format, torch.channels_last, torch.channels_last_3d]
LAYOUTS = [torch.strided, torch.sparse_coo]

T_NONE, T_TENSOR, T_INT, T_DOUBLE, T_BOOL, T_STRING, T_INTS, T_DOUBLES, T_BOOLS, T_TENSORS, \
    T_SCALAR_INT, T_SCALAR_DOUBLE, T_DEVICE, T_DTYPE, T_LAYOUT, T_MEMFMT, T_INLINE, T_TUPLE, T_OPT_INTS, T_SCALAR_BOOL = range(20)

DEVICE = "mps"


class Session:
    def __init__(self, device):
        self.device = torch.device(device)
        self.tensors = {}
        self.next = 1

    def put(self, t):
        h = self.next
        self.next += 1
        self.tensors[h] = t
        return h


def read_exact(c, n):
    b = bytearray()
    while len(b) < n:
        chunk = c.recv(min(n - len(b), 1 << 20))
        if not chunk:
            raise EOFError
        b += chunk
    return bytes(b)


class Reader:
    def __init__(self, buf):
        self.b, self.p = buf, 0

    def u8(self):
        v = self.b[self.p]; self.p += 1; return v

    def u32(self):
        (v,) = struct.unpack_from("<I", self.b, self.p); self.p += 4; return v

    def i64(self):
        (v,) = struct.unpack_from("<q", self.b, self.p); self.p += 8; return v

    def f64(self):
        (v,) = struct.unpack_from("<d", self.b, self.p); self.p += 8; return v

    def bytes(self):
        n = self.i64(); v = self.b[self.p:self.p + n]; self.p += n; return v

    def value(self, s):
        tag = self.u8()
        if tag == T_NONE: return None
        if tag == T_TENSOR:
            h = self.i64(); return s.tensors[h]
        if tag == T_INT: return self.i64()
        if tag == T_DOUBLE: return self.f64()
        if tag == T_BOOL: return bool(self.u8())
        if tag == T_STRING: return self.bytes().decode()
        if tag == T_INTS: n = self.u32(); return [self.i64() for _ in range(n)]
        if tag == T_DOUBLES: n = self.u32(); return [self.f64() for _ in range(n)]
        if tag == T_BOOLS: n = self.u32(); return [bool(self.u8()) for _ in range(n)]
        if tag == T_TENSORS: n = self.u32(); return [self.value(s) for _ in range(n)]
        if tag == T_SCALAR_INT: return self.i64()
        if tag == T_SCALAR_DOUBLE: return self.f64()
        if tag == T_SCALAR_BOOL: return bool(self.u8())
        if tag == T_DEVICE:
            kind, index = self.u8(), self.u8()
            # The guest's mps is this process's mps; the guest's cpu is ours.
            return s.device if kind == 13 else torch.device("cpu")
        if tag == T_DTYPE: return DTYPES[self.u8()]
        if tag == T_LAYOUT: return LAYOUTS[self.u8()]
        if tag == T_MEMFMT: return MEMORY_FORMATS[self.u8()]
        if tag == T_INLINE:
            dtype = DTYPES[self.u8()]; n = self.u32(); sizes = [self.i64() for _ in range(n)]; raw = self.bytes()
            t = torch.frombuffer(bytearray(raw), dtype=dtype).reshape(sizes) if len(raw) else torch.empty(sizes, dtype=dtype)
            return t.clone()
        if tag == T_TUPLE: n = self.u32(); return tuple(self.value(s) for _ in range(n))
        if tag == T_OPT_INTS:
            present = self.u8(); n = self.u32(); return [self.i64() for _ in range(n)] if present else None
        raise ValueError(f"unknown value tag {tag}")


class Writer:
    def __init__(self):
        self.parts = []

    def u8(self, v): self.parts.append(struct.pack("<B", v))
    def u32(self, v): self.parts.append(struct.pack("<I", v))
    def i64(self, v): self.parts.append(struct.pack("<q", v))
    def f64(self, v): self.parts.append(struct.pack("<d", v))
    def bytes(self, b): self.i64(len(b)); self.parts.append(bytes(b))

    def tensor(self, s, t, alias_of=None):
        """A tensor result: a handle plus the metadata the guest mirrors."""
        self.u8(T_TENSOR)
        self.i64(s.put(t))
        self.u8(DTYPE_INDEX[t.dtype])
        self.u32(t.dim()); [self.i64(x) for x in t.shape]
        self.u32(t.dim()); [self.i64(x) for x in t.stride()]
        self.i64(t.storage_offset())
        self.i64(t.untyped_storage().data_ptr())

    def value(self, s, v):
        if v is None: self.u8(T_NONE)
        elif isinstance(v, torch.Tensor):
            if v.device.type != s.device.type:
                v = v.to(s.device)
            self.tensor(s, v)
        elif isinstance(v, bool): self.u8(T_BOOL); self.u8(1 if v else 0)
        elif isinstance(v, int): self.u8(T_INT); self.i64(v)
        elif isinstance(v, float): self.u8(T_DOUBLE); self.f64(v)
        elif isinstance(v, str): self.u8(T_STRING); self.bytes(v.encode())
        elif isinstance(v, (list, tuple)) and all(isinstance(x, torch.Tensor) for x in v):
            self.u8(T_TENSORS); self.u32(len(v)); [self.value(s, x) for x in v]
        elif isinstance(v, (list, tuple)):
            self.u8(T_TUPLE); self.u32(len(v)); [self.value(s, x) for x in v]
        elif isinstance(v, torch.dtype): self.u8(T_DTYPE); self.u8(DTYPE_INDEX[v])
        elif isinstance(v, torch.SymInt): self.u8(T_INT); self.i64(int(v))
        else: raise TypeError(f"cannot encode result of type {type(v)}")

    def data(self):
        return b"".join(self.parts)


def resolve_op(name):
    """`aten::add.Tensor` -> torch.ops.aten.add.Tensor."""
    ns, rest = name.split("::", 1)
    base, _, overload = rest.partition(".")
    packet = getattr(getattr(torch.ops, ns), base)
    return getattr(packet, overload or "default")


def handle(kind, payload, s):
    r = Reader(payload)
    w = Writer()
    if kind == HELLO:
        guest = r.bytes().decode()
        host = torch.__version__
        if guest.split("+")[0].rsplit(".", 1)[0] != host.split("+")[0].rsplit(".", 1)[0]:
            raise RuntimeError(f"the container's torch {guest} does not match the Mac's {host}; they must share a major.minor")
        w.bytes(host.encode()); w.bytes(DEVICE.encode())
        return w.data()
    if kind == OP:
        name = r.bytes().decode()
        n = r.u32()
        args = [r.value(s) for _ in range(n)]
        op = resolve_op(name)
        out = op(*args)
        # Aliases: an output that IS one of the arguments (in-place, out=)
        # goes back as that argument's position so the guest keeps its object.
        # A tuple is several returns; anything else, a list included, is one.
        outs = list(out) if isinstance(out, tuple) else [out]
        w.u32(len(outs))
        for o in outs:
            if isinstance(o, torch.Tensor):
                for i, a in enumerate(args):
                    if a is o:
                        w.u8(0xFE); w.u32(i); break
                else:
                    w.u8(0xFF); w.tensor(s, o)
            else:
                w.u8(0xFF); w.value(s, o)
        return w.data()
    if kind == UPLOAD:
        dtype = DTYPES[r.u8()]
        n = r.u32(); sizes = [r.i64() for _ in range(n)]
        raw = r.bytes()
        t = (torch.frombuffer(bytearray(raw), dtype=dtype).reshape(sizes) if len(raw) else torch.empty(sizes, dtype=dtype)).to(s.device)
        w.tensor(s, t)
        return w.data()
    if kind == DOWNLOAD:
        h = r.i64()
        t = s.tensors[h].contiguous().cpu()
        w.bytes(t.numpy().tobytes() if t.dtype != torch.bfloat16 else t.view(torch.int16).numpy().tobytes())
        return w.data()
    if kind == FREE:
        n = r.u32()
        for _ in range(n):
            s.tensors.pop(r.i64(), None)
        return b""
    raise ValueError(f"unknown request {kind}")


def serve(c, device):
    s = Session(device)
    try:
        while True:
            try:
                hdr = read_exact(c, 12)
            except EOFError:
                return
            kind, n = struct.unpack("<IQ", hdr)
            payload = read_exact(c, n)
            try:
                body = handle(kind, payload, s)
                c.sendall(struct.pack("<IQ", kind, len(body)) + body)
            except Exception as e:  # noqa: BLE001 - every failure goes back to the guest
                msg = f"{type(e).__name__}: {e}".encode()
                c.sendall(struct.pack("<IQ", ERR, len(msg)) + msg)
    finally:
        c.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--device", default="mps")
    args = ap.parse_args()
    global DEVICE
    DEVICE = args.device
    if DEVICE == "mps" and not torch.backends.mps.is_available():
        print("torch has no MPS on this machine", file=sys.stderr); sys.exit(2)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", args.port))
    srv.listen(16)
    print(f"PORT {srv.getsockname()[1]}", flush=True)
    while True:
        c, _ = srv.accept()
        c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        threading.Thread(target=serve, args=(c, DEVICE), daemon=True).start()


if __name__ == "__main__":
    main()

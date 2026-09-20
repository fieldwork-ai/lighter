# Spike stand-in for the host side: the same protocol the VMM will speak, backed
# by ONNX Runtime on this machine (CPU here; CoreML on the Mac).
import os, socket, struct, sys, numpy as np, onnxruntime as ort
PATH = os.environ.get("LIGHTER_ANE_SOCKET", "/tmp/lighter-ane.sock")
PROVIDERS = [p for p in sys.argv[1:]] or ["CPUExecutionProvider"]
NP = {1: np.float32, 2: np.uint8, 3: np.int8, 4: np.uint16, 5: np.int16, 6: np.int32, 7: np.int64, 9: np.bool_, 10: np.float16, 11: np.float64, 12: np.uint32, 13: np.uint64}
ET = {v: k for k, v in NP.items()}
def recv_exact(c, n):
    b = bytearray()
    while len(b) < n:
        chunk = c.recv(n - len(b))
        if not chunk: raise EOFError
        b += chunk
    return bytes(b)
def serve(c):
    sessions = {}
    while True:
        try: hdr = recv_exact(c, 12)
        except EOFError: return
        kind, n = struct.unpack("<IQ", hdr); body = recv_exact(c, n)
        try:
            if kind == 1:
                so = ort.SessionOptions(); so.log_severity_level = 3
                s = ort.InferenceSession(body, so, providers=PROVIDERS)
                sid = len(sessions) + 1; sessions[sid] = s
                print(f"[host] loaded session {sid}: {len(body)} bytes, providers={s.get_providers()}, inputs={[i.name for i in s.get_inputs()]}", flush=True)
                reply = struct.pack("<Q", sid)
            elif kind == 2:
                sid, nin = struct.unpack_from("<QI", body); p = 12; s = sessions[sid]; feeds = {}
                for i, meta in enumerate(s.get_inputs()):
                    et, nd = struct.unpack_from("<II", body, p); p += 8
                    dims = struct.unpack_from("<%dq" % nd, body, p); p += 8 * nd
                    (size,) = struct.unpack_from("<Q", body, p); p += 8
                    feeds[meta.name] = np.frombuffer(body[p:p+size], dtype=NP[et]).reshape(dims); p += size
                outs = s.run(None, feeds)
                reply = struct.pack("<I", len(outs))
                for o in outs:
                    o = np.ascontiguousarray(o); raw = o.tobytes()
                    reply += struct.pack("<II", ET[o.dtype.type], o.ndim) + struct.pack("<%dq" % o.ndim, *o.shape) + struct.pack("<Q", len(raw)) + raw
            else:
                raise ValueError(f"unknown kind {kind}")
            c.sendall(struct.pack("<IQ", kind, len(reply)) + reply)
        except Exception as e:
            msg = str(e).encode(); c.sendall(struct.pack("<IQ", 0xffff, len(msg)) + msg)
if os.path.exists(PATH): os.unlink(PATH)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); srv.bind(PATH); srv.listen(8)
print(f"[host] listening on {PATH} with {PROVIDERS}", flush=True)
while True:
    c, _ = srv.accept()
    import threading; threading.Thread(target=serve, args=(c,), daemon=True).start()

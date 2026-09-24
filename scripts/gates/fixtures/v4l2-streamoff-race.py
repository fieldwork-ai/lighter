#!/usr/bin/env python3
"""Drive the decoder through CAPTURE STREAMOFF/STREAMON cycles while frames
complete: the path where a completion the device sent before a STREAMOFF is
handled after it, which once put a buffer on the guest driver's done list
twice and oopsed the next DQBUF (kernel patch 0042). Gate m13 runs many at
once under load.

  v4l2-streamoff-race.py <annexb.h264> <seconds> <width> <height>"""
import ctypes, errno, fcntl, mmap, os, select, sys, time

VIDEO = "/dev/video0"
OUT, CAP, MMAP = 10, 9, 1  # OUTPUT_MPLANE, CAPTURE_MPLANE, MEMORY_MMAP
# Dequeued frames between CAPTURE restarts.
EVERY = int(os.environ.get("RACE_EVERY", "7"))
# CAPTURE restarts in a row each time.
BURST = int(os.environ.get("RACE_BURST", "4"))
# CAPTURE buffers per client.
CAPTURE = int(os.environ.get("RACE_CAPTURE", "12"))


def ioc(d, nr, size):
    return (d << 30) | (size << 16) | (ord("V") << 8) | nr


class PlaneFmt(ctypes.Structure):
    _fields_ = [("sizeimage", ctypes.c_uint32), ("bytesperline", ctypes.c_uint32), ("reserved", ctypes.c_uint16 * 6)]


class PixMp(ctypes.Structure):
    _fields_ = [("width", ctypes.c_uint32), ("height", ctypes.c_uint32), ("pixelformat", ctypes.c_uint32),
                ("field", ctypes.c_uint32), ("colorspace", ctypes.c_uint32), ("plane_fmt", PlaneFmt * 8),
                ("num_planes", ctypes.c_uint8), ("flags", ctypes.c_uint8), ("ycbcr_enc", ctypes.c_uint8),
                ("quantization", ctypes.c_uint8), ("xfer_func", ctypes.c_uint8), ("reserved", ctypes.c_uint8 * 7)]


class FmtUnion(ctypes.Union):
    _fields_ = [("pix_mp", PixMp), ("raw", ctypes.c_uint8 * 200), ("align", ctypes.c_uint64)]


class Format(ctypes.Structure):
    _fields_ = [("type", ctypes.c_uint32), ("fmt", FmtUnion)]


class ReqBufs(ctypes.Structure):
    _fields_ = [("count", ctypes.c_uint32), ("type", ctypes.c_uint32), ("memory", ctypes.c_uint32),
                ("capabilities", ctypes.c_uint32), ("flags", ctypes.c_uint8), ("reserved", ctypes.c_uint8 * 3)]


class Plane(ctypes.Structure):
    _fields_ = [("bytesused", ctypes.c_uint32), ("length", ctypes.c_uint32), ("mem_offset", ctypes.c_uint64),
                ("data_offset", ctypes.c_uint32), ("reserved", ctypes.c_uint32 * 11)]


class Buffer(ctypes.Structure):
    _fields_ = [("index", ctypes.c_uint32), ("type", ctypes.c_uint32), ("bytesused", ctypes.c_uint32),
                ("flags", ctypes.c_uint32), ("field", ctypes.c_uint32), ("timestamp", ctypes.c_int64 * 2),
                ("timecode", ctypes.c_uint8 * 16), ("sequence", ctypes.c_uint32), ("memory", ctypes.c_uint32),
                ("planes", ctypes.c_void_p), ("length", ctypes.c_uint32), ("reserved2", ctypes.c_uint32),
                ("request_fd", ctypes.c_int32)]


assert ctypes.sizeof(Format) == 208 and ctypes.sizeof(Buffer) == 88 and ctypes.sizeof(Plane) == 64
S_FMT, REQBUFS, QUERYBUF = ioc(3, 5, 208), ioc(3, 8, 20), ioc(3, 9, 88)
QBUF, DQBUF = ioc(3, 15, 88), ioc(3, 17, 88)
STREAMON, STREAMOFF = ioc(1, 18, 4), ioc(1, 19, 4)


def fourcc(s):
    return int.from_bytes(s.encode(), "little")


def access_units(data):
    """Annex B split into access units: parameter sets ride with the next slice."""
    starts = [i for i in range(len(data) - 3) if data[i:i + 3] == b"\0\0\1"]
    nals = [data[s:e] for s, e in zip(starts, starts[1:] + [len(data)])]
    units, cur = [], b""
    for n in nals:
        cur += b"\0" + n
        if n[3] & 0x1F in (1, 5):
            units.append(cur)
            cur = b""
    return units


def main():
    units = access_units(open(sys.argv[1], "rb").read())
    end = time.time() + float(sys.argv[2])
    width, height = int(sys.argv[3]), int(sys.argv[4])
    fd = os.open(VIDEO, os.O_RDWR | os.O_NONBLOCK)

    def ctl(req, arg):
        fcntl.ioctl(fd, req, arg)

    for t, pf in ((OUT, "H264"), (CAP, "NV12")):
        f = Format(type=t)
        f.fmt.pix_mp.width, f.fmt.pix_mp.height = width, height
        f.fmt.pix_mp.pixelformat, f.fmt.pix_mp.num_planes = fourcc(pf), 1
        if t == OUT:
            f.fmt.pix_mp.plane_fmt[0].sizeimage = 512 << 10
        ctl(S_FMT, f)

    maps = {}
    for t, n in ((OUT, 8), (CAP, CAPTURE)):
        rb = ReqBufs(count=n, type=t, memory=MMAP)
        ctl(REQBUFS, rb)
        maps[t] = []
        for i in range(rb.count):
            p = Plane()
            b = Buffer(index=i, type=t, memory=MMAP, length=1, planes=ctypes.addressof(p))
            ctl(QUERYBUF, b)
            maps[t].append((mmap.mmap(fd, p.length, offset=p.mem_offset), p.length))

    def qbuf(t, i, used=0):
        p = Plane(bytesused=used, length=maps[t][i][1])
        ctl(QBUF, Buffer(index=i, type=t, memory=MMAP, length=1, planes=ctypes.addressof(p)))

    def dqbuf(t):
        p = Plane()
        b = Buffer(type=t, memory=MMAP, length=1, planes=ctypes.addressof(p))
        try:
            ctl(DQBUF, b)
        except OSError as e:
            if e.errno in (errno.EAGAIN, errno.EPIPE, errno.EINVAL):
                return None
            raise
        return b.index, b.flags

    free_out = list(range(len(maps[OUT])))
    for i in range(len(maps[CAP])):
        qbuf(CAP, i)
    ctl(STREAMON, ctypes.c_int(OUT))
    ctl(STREAMON, ctypes.c_int(CAP))

    u = frames = cycles = 0
    while time.time() < end:
        while free_out:
            i = free_out.pop()
            au = units[u % len(units)]
            u += 1
            maps[OUT][i][0][:len(au)] = au
            qbuf(OUT, i, len(au))
        select.select([fd], [], [], 0.01)
        while (r := dqbuf(OUT)) is not None:
            free_out.append(r[0])
        last = False
        while (r := dqbuf(CAP)) is not None:
            frames += 1
            if r[1] & 0x00100000:  # V4L2_BUF_FLAG_LAST: restart CAPTURE
                last = True
                break
            qbuf(CAP, r[0])
        if last or (frames and frames % EVERY == 0):
            # Back to back: queueing CAPTURE hands the device frames it has
            # already decoded, so its completions are in flight when the
            # next STREAMOFF goes out, which is the window.
            for _ in range(BURST):
                ctl(STREAMOFF, ctypes.c_int(CAP))
                ctl(STREAMON, ctypes.c_int(CAP))
                for i in range(len(maps[CAP])):
                    qbuf(CAP, i)
                cycles += 1
            frames += 1
    print(f"frames {frames} streamoff cycles {cycles}", flush=True)


main()

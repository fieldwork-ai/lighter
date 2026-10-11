"""Write an H.264 stream in v4l2-compliance's --stream-from-hdr format.

    v4l2-stream-hdr.py <in.h264> <out.hdr>

Each record is one access unit, the unit a stateful decoder's OUTPUT buffer
carries: v4l2_fourcc('V','h','d','r') and the unit's length, both big-endian,
then its bytes. Given no stream, v4l2-compliance streams empty buffers, which
no decoder can parse into the source change event it then waits for.
"""

import struct
import sys

FILE_HDR_ID = struct.pack(">I", 0x72646856)  # v4l2_fourcc('V', 'h', 'd', 'r')
VCL = (1, 5)


def nals(data):
    """Yields each NAL unit's start (at its start code), type and first byte after the header."""
    i = 0
    while True:
        i = data.find(b"\x00\x00\x01", i)
        if i < 0 or i + 3 >= len(data):
            return
        start = i - 1 if i > 0 and data[i - 1] == 0 else i
        yield start, data[i + 3] & 0x1F, data[i + 4] if i + 4 < len(data) else 0
        i += 3


def access_units(data):
    starts, in_picture = [], False
    for start, kind, first in nals(data):
        # A new unit at an access unit delimiter, at parameter sets or SEI
        # after a picture, and at a picture's first slice (first_mb_in_slice
        # is 0, coded as a single 1 bit) after a picture.
        first_slice = kind in VCL and first & 0x80
        if kind == 9 or (in_picture and (kind in (6, 7, 8) or first_slice)) or not starts:
            starts.append(start)
            in_picture = False
        if kind in VCL:
            in_picture = True
    starts.append(len(data))
    return [data[a:b] for a, b in zip(starts, starts[1:]) if b > a]


def main():
    with open(sys.argv[1], "rb") as f:
        units = access_units(f.read())
    with open(sys.argv[2], "wb") as f:
        for unit in units:
            f.write(FILE_HDR_ID + struct.pack(">I", len(unit)) + unit)
    print(f"{len(units)} access units")


if __name__ == "__main__":
    main()

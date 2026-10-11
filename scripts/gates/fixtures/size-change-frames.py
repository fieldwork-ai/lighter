"""Compare a hardware decode of a stream that changes size with software's.

    size-change-frames.py <software.frames> <hardware.frames>

Each file has one line per frame, "<size> <hash>", from ffmpeg's framemd5.
Prints "exact" when the two match, and "dropped <n>" when the hardware decode
lacks only the last n frames before the size change: ffmpeg's V4L2 wrapper
turns CAPTURE off as soon as it reads the change, before dequeuing the frames
decoded ahead of it (FFmpeg/FFmpeg#25012), which no device can prevent; it
can lose no more than its CAPTURE buffers hold. Any other difference prints
"differ: ..." and exits 1: a frame of the new size lost, or any frame changed,
is the device's.
"""

import sys

# ffmpeg's num_capture_buffers default.
CAPTURE_BUFFERS = 20


def frames(path):
    with open(path) as f:
        return [tuple(line.split()) for line in f if line.strip()]


def main():
    sw, hw = frames(sys.argv[1]), frames(sys.argv[2])
    if hw == sw:
        print("exact")
        return 0
    change = next((i for i in range(1, len(sw)) if sw[i][0] != sw[i - 1][0]), None)
    if change is None:
        print(f"differ: the software decode has no size change ({len(sw)} frames)")
        return 1
    dropped = len(sw) - len(hw)
    if 0 < dropped <= min(change, CAPTURE_BUFFERS) and hw == sw[: change - dropped] + sw[change:]:
        print(f"dropped {dropped}")
        return 0
    first = next((i for i, (a, b) in enumerate(zip(sw, hw)) if a != b), min(len(sw), len(hw)))
    print(f"differ: {len(hw)} of {len(sw)} frames, first difference at frame {first + 1}, size change at frame {change + 1}")
    return 1


if __name__ == "__main__":
    sys.exit(main())

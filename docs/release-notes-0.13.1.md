# lighter 0.13.1

A video stream whose picture gets smaller as it plays now goes on decoding on your Mac's media engine, at its new size, instead of stopping where the size changed.

## What went wrong

**A stream that shrank stopped at the change.** A camera whose resolution is lowered while it streams, or a recording that switches to a smaller picture, sends its new size mid-stream. lighter's decoder reported a larger size as changed, but a smaller one as the old size, because it kept CAPTURE's buffers at the largest size it had seen. Debian's ffmpeg (`h264_v4l2m2m`), told nothing had changed, read the end of the old picture size as the end of the stream: a 640x360 clip that turned 320x180 gave 90 of its 120 frames, and exit code 0.

Reporting the new size properly brought two more problems to light, both fixed:

- **Frames could reach buffers the guest was about to discard.** After a size change, a decoder must stop until the guest has set up buffers for the new size. lighter went on, and once a picture shrank, the old, larger buffers were big enough for it, so new frames could go into them and be thrown away with them.
- **The guest kernel told ffmpeg the device had failed in the middle of the change.** Its video driver reported an error from `poll` whenever the decoder's output queue was off, which it is while a client switches to new buffers. ffmpeg, polling at that moment, looks through the buffers it has just freed and crashes. In our tests 0.13.0 never reached that moment; the corrected decoder did.

## What changed

- **After a size change, the decoder reports the stream's new size, smaller or larger,** and stops at the end of the old size until the guest has set up for the new one, as the V4L2 standard describes; frames of the new size go only into buffers made for it.
- **The guest kernel's video driver reports an error only when neither of the decoder's queues can make progress,** as Linux's own decoders do.

ffmpeg decodes a 640x360 stream that turns 320x180, and one that does the reverse, to the same 120 frames, at both sizes, as its software decoder.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with the video driver change (kernel bf15cbbd). The data epoch remains **1**.

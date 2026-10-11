# lighter 0.13.2

Video decoding and encoding in containers now do everything Linux's video interface promises programs, checked by its own conformance suite: a stream that changes size no longer loses the first frame of its new size, and programs that follow that interface to the letter, such as `v4l2-ctl`, now decode every frame instead of stopping after the first few.

## What went wrong

**A stream that changed size could lose a frame at the change.** After a size change, a decoder must wait until the program has set up for the new size before handing it any more frames. lighter handed the first frame of the new size to the first buffer the program gave back, even one it was about to throw away while it switched. ffmpeg's current development version gives one back at exactly that moment, and lost that frame almost every time.

**Some programs stopped after four frames.** A program may hand the decoder its buffers without saying how big they are, since the decoder made them and knows. lighter took the size it was given, 0, handed it back with the first frame, and refused the buffer when the program reused it. `v4l2-ctl` does this, and decoded four frames, then nothing.

**A program that set up its buffers twice could stall for good.** When a program asks for a fresh set of buffers, everything queued in the old set goes. The guest kernel's video driver did that only when asked for no buffers at all, so after asking for a new set it kept counting buffers that no longer existed and never told the program it had room for more.

Along with these, the conformance suite found smaller departures, all fixed: two settings the devices do not support answered "not supported" for some requests and "invalid" for others, a buffer could be queued twice, a buffer queued before decoding started was decoded at once, fresh buffers came back still marked with an old stream's state, and buffers were numbered and timestamped differently from what Linux's own drivers do.

## What changed

- **After a size change, the decoder gives frames of the new size only to buffers the program has queued for it,** once it has set up for the new size, as the interface requires.
- **The guest kernel's video driver** turns off the settings a device does not support, refuses to queue a buffer twice, and starts a queue afresh whenever a program asks for new buffers, as Linux's own drivers do.
- **The decoder and encoder** keep the buffer sizes they chose, hold buffers until the program starts decoding, and number and timestamp buffers as Linux's own drivers do.

`v4l2-compliance`, Linux's conformance suite for video devices, passes both devices whole, streaming tests included.

ffmpeg itself can still lose the last frames before a size change: it switches to the new size as soon as it hears of it, before collecting the frames already decoded, on any Linux decoder. The fix is with ffmpeg's developers ([FFmpeg/FFmpeg#25012](https://code.ffmpeg.org/FFmpeg/FFmpeg/pulls/25012)).

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with the video driver changes (kernel a94fa71c). The data epoch remains **1**.

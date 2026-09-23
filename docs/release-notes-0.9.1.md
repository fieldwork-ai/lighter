# lighter 0.9.1

The Neural Engine works with the ONNX Runtime a container already has, both video devices answer the way V4L2 promises, and `lighter stop` hands the Docker CLI back to the context you were using.

## The Neural Engine from ONNX Runtime 1.23

The plugin provider behind `--device lighter.sh/ane=all` was built against ONNX Runtime 1.30's API, so an older runtime refused to load it ("requested API version [30] is not available"), even though plugin providers exist from 1.23. It is now built against 1.23's API, and a runtime serves any API up to its own, so every ONNX Runtime from 1.23 on loads it. The one thing 1.23 lacks is a way for a provider to name a device of its own, which came in 1.25, so the Neural Engine is offered on the CPU-type device ONNX Runtime passes in; `ort.get_ep_devices()` lists it as `LighterANE`, as before. Gate `m10` runs its models on 1.23 as well as on the newest release. `guest/ane-ep/bindgen.sh` regenerates the bindings.

## V4L2 compliance

`v4l2-compliance` is the kernel's own check that a device does what V4L2 promises clients. On 0.9.0 the decoder failed 6 of its 48 tests and the encoder 12. The encoder now passes all 48 and the decoder 47. What was wrong:

- Controls. Neither device announced the class its controls belong to, both let a client "set" a read-only control, the encoder's key-frame button reported a range, and neither took a request for no controls or for one class. Both devices now answer controls through one implementation of the kernel's control-framework rules (`third_party/virtio-media/src/controls.rs`), including which entry an error names, and both deliver `V4L2_EVENT_CTRL`, with the initial value when asked. The decoder no longer has an output minimum-buffers control, which only encoders have.
- The encoder offered frame intervals for sizes it does not take, had a frame rate on its CAPTURE queue, accepted a crop on read-only selection targets, and dropped the colorimetry a client set. Colorimetry now carries from OUTPUT to CAPTURE; it is not yet written into the encoded stream.
- Neither device may say it honours `V4L2_MEMORY_FLAG_NON_COHERENT`, having no cache hints, and both did.

The decoder's remaining failure is the guest driver's rather than the device's: virtio-media takes `VIDIOC_G_PARM` for every device, so the kernel refuses a bad buffer type with EINVAL before the device can answer ENOTTY, which a stateful decoder must. No client depends on it. Gate `m14` now runs `v4l2-compliance` on both nodes and allows exactly that one failure.

## A container could stop the machine

Two sequences of ioctls from a container with the video device panicked lighter's process, taking every container with it: subscribing to an event type the device did not know, and queueing a decode buffer, then shrinking the queue with `VIDIOC_REQBUFS` before streaming. Both were reproduced on 0.9.0 and now fail the ioctl or carry on. The decoder also took any CAPTURE size a client set, up to four billion pixels a side, and multiplied it into buffer sizes that wrapped; a frame copied with such a size would have indexed past its buffer, which aborts. It now holds the size to the 16 to 8192 it offers. The decoder's crop also computed its height from the width, and could underflow on a crop that started past its end.

## `lighter stop` restores your Docker context

`lighter start` selects the `lighter` context, and `lighter stop` used to select `default`, whatever was in use before. It now goes back to the context that was selected when lighter started, if that context still exists and its daemon answers within three seconds, and otherwise to `default`. If you have selected something else since `lighter start`, stop leaves it alone. `lighter restart` no longer touches the context at all.

## Also

- Guest patch 0041 is now a fix to the V4L2 core itself, zeroing the extended control it builds for `VIDIOC_G_CTRL` and `VIDIOC_S_CTRL`, in place of the driver workaround 0.9.0 carried. It has been sent upstream (https://lore.kernel.org/all/20260923160936.33445-1-nick@getfieldwork.ai/), and the driver fixes 0038, 0040 and the original cause of 0041 have been reported on the virtio-media v9 thread.
- Linux remains **6.18.52**, with guest patches 0036 to 0041; the data epoch remains **1**.

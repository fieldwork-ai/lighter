# whisper.cpp on the Mac's GPU, for Home Assistant

whisper.cpp built with ggml's RPC backend, so the encoder and decoder run on
the Mac's GPU through `lighter.sh/metal`, with a Wyoming speech-to-text
server in front of it that Home Assistant's Assist uses as any other.

Two things the image arranges. whisper.cpp's bundled ggml is one RPC
protocol version behind lighter's server, so the ggml tree comes from the
llama.cpp commit lighter builds against. And whisper.cpp has no `--rpc`
flag: a header-only helper (`0001-rpc-servers-from-env.patch`, forty lines)
registers the server named by `WHISPER_RPC` or by the device's own
`LIGHTER_METAL` as a GPU device before the model loads. One trap in it:
`ggml_backend_rpc_add_server` returns a registry, not a device, and the
registry's devices must be registered with ggml's global list before the
model loader can see them.

```bash
docker build -t whisper-metal .
docker run -d --name whisper --restart unless-stopped \
  --device lighter.sh/metal=all -v ./models:/models \
  -e MODEL=/models/ggml-small.en.bin -p 10300:10300 whisper-metal
```

Models are whisper.cpp's ggml files (`ggml-small.en.bin` from
`huggingface.co/ggerganov/whisper.cpp`). In Home Assistant, add the Wyoming
integration pointing at port 10300 and pick it as the speech-to-text engine
of an Assist pipeline.

Transcribing whisper.cpp's 11 s JFK sample with `whisper-cli`, wall time
including the model load, model on a docker volume, caches warm, on an M1:

| model | container, `lighter.sh/metal` | container, CPU | native Metal |
|---|---:|---:|---:|
| base.en | 0.45 s | 1.46 s | 0.38 s |
| small.en | 1.30 s | 5.80 s | 1.36 s |

The first run after a boot pays the read of the model file from disk on top
(0.5 s for base.en, 1.5 s for small.en, as native does with a cold cache).
Through Home Assistant's Wyoming path, 3.5 s of speech comes back as text in
0.5 s with small.en.

# whisper.cpp with ggml's RPC backend, for lighter.sh/metal: the encoder and
# decoder run on the Mac's GPU through lighter's in-process ggml server.
# whisper.cpp's own ggml is a protocol version behind, so the ggml tree comes
# from the llama.cpp commit lighter builds against.
FROM debian:trixie-slim AS build
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends git build-essential cmake ninja-build ca-certificates patch >/dev/null
ARG WHISPER_COMMIT=5670d5c0
ARG GGML_FROM_LLAMA=1af554f
RUN git clone -q https://github.com/ggml-org/whisper.cpp /src && git -C /src checkout -q $WHISPER_COMMIT \
 && git clone -q https://github.com/ggml-org/llama.cpp /llama && git -C /llama checkout -q $GGML_FROM_LLAMA \
 && rm -rf /src/ggml && cp -r /llama/ggml /src/ggml
COPY 0001-rpc-servers-from-env.patch /tmp/
RUN cd /src && git apply /tmp/0001-rpc-servers-from-env.patch && test -f examples/rpc-env.h
RUN cd /src && cmake -B build -G Ninja -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DGGML_RPC=ON -DGGML_NATIVE=OFF -DCMAKE_BUILD_TYPE=Release -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON 2>&1 | tail -20 \
 && cmake --build build --target whisper-server whisper-bench whisper-cli -j 2>&1 | grep -E "error|Error|FAILED" -A3 | head -40; test ${PIPESTATUS:-0} -eq 0 && test -x build/bin/whisper-server

FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends python3 python3-pip ca-certificates curl libgomp1 >/dev/null \
 && pip3 install -q --break-system-packages --root-user-action=ignore wyoming
COPY --from=build /src/build/bin/whisper-server /src/build/bin/whisper-bench /src/build/bin/whisper-cli /usr/local/bin/
COPY --from=build /src/build/bin/*.so* /usr/local/lib/
RUN ldconfig
COPY wyoming_whispercpp.py /usr/local/bin/wyoming-whispercpp
COPY entrypoint.sh /usr/local/bin/entrypoint
RUN chmod +x /usr/local/bin/wyoming-whispercpp /usr/local/bin/entrypoint
EXPOSE 10300 8080
ENTRYPOINT ["/usr/local/bin/entrypoint"]

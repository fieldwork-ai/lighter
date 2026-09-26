# The image every containerized benchmark target runs in.
#
# The package managers are pinned to the versions the native target has, so a
# comparison against macOS measures the filesystem rather than two different
# installers; `report.py` prints both sets of versions.
#
# `--force` because the node image ships its own `yarn` shim in
# /usr/local/bin and npm refuses to replace a file it did not create. Pinning
# our own over it is the point.
# llama-bench for the llm case, CPU only, from a pinned release: the same
# arithmetic in every runtime, so the case measures the vCPUs and memory
# each runtime gives a container, not a GPU only some of them have.
FROM node:24.18.0-alpine@sha256:a0b9bf06e4e6193cf7a0f58816cc935ff8c2a908f81e6f1a95432d679c54fbfd AS llama
RUN apk add --no-cache build-base cmake git linux-headers \
	&& git clone --depth 1 --branch b11191 https://github.com/ggml-org/llama.cpp /src \
	&& cmake -S /src -B /build -DGGML_NATIVE=OFF -DGGML_CPU_ARM_ARCH=armv8.2-a+dotprod -DBUILD_SHARED_LIBS=OFF -DLLAMA_CURL=OFF \
	&& cmake --build /build --target llama-bench -j

FROM node:24.18.0-alpine@sha256:a0b9bf06e4e6193cf7a0f58816cc935ff8c2a908f81e6f1a95432d679c54fbfd

RUN apk add --no-cache ripgrep git tar iperf3 ffmpeg libstdc++ libgomp \
	&& npm install -g --force --no-audit --no-fund npm@11.16.0 pnpm@10.28.0 yarn@1.22.22
COPY --from=llama /build/bin/llama-bench /usr/local/bin/llama-bench

WORKDIR /work

# llama.cpp built with ggml's RPC backend (the client of lighter.sh/metal) and
# its Vulkan backend (lighter.sh/gpu, through Mesa's Venus driver), for gate
# m12 and benchmarks/llm-gpu.sh. LLAMA_COMMIT is the commit lighter's Metal
# server is built from (host/metal/build.sh): the RPC protocol is checked by
# version, so the client and the server are one commit.
#
#   scripts/gates/llama-image.sh builds it, once, and keeps it as a tarball.
FROM debian:trixie@sha256:9cc080028c43b27d2074d63a5f9caf7166d731494965616c1a6d2827a004585c AS build
ARG LLAMA_COMMIT
RUN apt-get update -qq \
	&& apt-get install -y -qq --no-install-recommends build-essential cmake git ca-certificates libvulkan-dev glslc spirv-headers >/dev/null \
	&& git clone -q https://github.com/ggml-org/llama.cpp /src \
	&& git -C /src checkout -q "$LLAMA_COMMIT" \
	&& cmake -S /src -B /src/build -DGGML_RPC=ON -DGGML_VULKAN=ON -DGGML_NATIVE=OFF \
		-DGGML_CPU_ARM_ARCH=armv8.2-a+dotprod -DLLAMA_CURL=OFF -DCMAKE_BUILD_TYPE=Release \
	&& cmake --build /src/build --target llama-bench llama-cli llama-simple -j 2

FROM debian:trixie@sha256:9cc080028c43b27d2074d63a5f9caf7166d731494965616c1a6d2827a004585c
RUN apt-get update -qq \
	&& apt-get install -y -qq --no-install-recommends libvulkan1 mesa-vulkan-drivers vulkan-tools libgomp1 ca-certificates curl >/dev/null \
	&& rm -rf /var/lib/apt/lists/*
COPY --from=build /src/build/bin/llama-bench /src/build/bin/llama-cli /src/build/bin/llama-simple /usr/local/bin/
COPY --from=build /src/build/bin/*.so* /usr/local/lib/
RUN ldconfig
WORKDIR /models

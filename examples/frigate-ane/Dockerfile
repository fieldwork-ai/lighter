# Frigate with a detector on the Mac's Neural Engine, through lighter.
#
# Frigate 0.18 ships ONNX Runtime 1.18, which predates the plugin provider
# API (1.23); the runtime is upgraded and one detector plugin added.
FROM ghcr.io/blakeblackshear/frigate:stable
RUN pip3 install --break-system-packages --root-user-action=ignore -q "onnxruntime==1.30.*"
COPY lighter_ane.py /opt/frigate/frigate/detectors/plugins/lighter_ane.py

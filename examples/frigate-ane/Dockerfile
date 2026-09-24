# Frigate with a detector on the Mac's Neural Engine, through lighter.
#
# Frigate's own image as it ships, ONNX Runtime included, with one detector
# plugin added: lighter's library runs as an ONNX Runtime custom op, which any
# ONNX Runtime from 1.16 loads.
FROM ghcr.io/blakeblackshear/frigate:stable
COPY lighter_ane.py /opt/frigate/frigate/detectors/plugins/lighter_ane.py

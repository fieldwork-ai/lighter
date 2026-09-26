"""Immich's machine learning on the Mac's Neural Engine, through lighter.sh/ane.

Python imports this module into every process it starts, gunicorn's workers
included, when its directory is on PYTHONPATH. Immich builds each ONNX Runtime
session with its own list of providers, which cannot name one ONNX Runtime
loads as a plugin; so the session class is wrapped, and a session asked for
the CPU alone is given lighter's provider ahead of it. Nodes lighter's provider
does not claim, and any model it cannot take, stay on the container's CPU as
before. Nothing happens without the device (`LIGHTER_ANE` unset) or on an ONNX
Runtime older than 1.23, the first with plugin providers.
"""

import os
import sys

LIBRARY = "/usr/lib/lighter/liblighter_ane_ep.so"


def _install():
    import onnxruntime as ort

    ort.register_execution_provider_library("lighter", LIBRARY)
    devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
    if not devices:
        return
    session = ort.InferenceSession

    class InferenceSession(session):
        def __init__(self, path_or_bytes, sess_options=None, providers=None, provider_options=None, **kwargs):
            if providers in (None, ["CPUExecutionProvider"]):
                sess_options = sess_options or ort.SessionOptions()
                # The devices choose the providers; a list besides them is refused.
                sess_options.add_provider_for_devices(devices, {})
                providers, provider_options = None, None
            super().__init__(path_or_bytes, sess_options, providers, provider_options, **kwargs)

    ort.InferenceSession = InferenceSession


if os.environ.get("LIGHTER_ANE") and os.path.exists(LIBRARY):
    try:
        _install()
    except Exception as error:  # a model on the CPU is better than no Immich
        print(f"lighter.sh/ane: not used ({error})", file=sys.stderr)

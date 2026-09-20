"""`torch.device("mps")` inside a container, run by the Mac's own PyTorch.

Importing this module is all a program has to do; `model.to("mps")` then
works as it would on the Mac. The container needs `--device lighter.sh/mps=all`.
"""
import torch  # noqa: F401  (must be imported before the extension)

from . import _lighter_mps  # noqa: F401  registers the backend

if not torch.backends.mps.is_available():
    # The hooks were registered after torch cached its answer.
    torch.backends.mps.is_available = lambda: True

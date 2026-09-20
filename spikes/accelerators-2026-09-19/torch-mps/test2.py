import torch, sys, traceback
sys.path.insert(0, "/tmp/torch_extensions")
import build  # noqa
torch.backends.mps.is_available = lambda: True
x = torch.arange(6., device="mps").reshape(2, 3)
for name, fn in [("add", lambda: x + 1), ("sum", lambda: x.sum()), ("relu", lambda: x.relu()),
                 ("mm", lambda: x @ x.T), ("mul", lambda: x * x), ("item", lambda: x.sum().item()),
                 ("cpu", lambda: x.cpu()), ("linear", lambda: torch.nn.Linear(3, 2).to("mps")(x))]:
    try:
        r = fn(); print(name, "OK", getattr(r, "device", r))
    except Exception as e:
        print(name, "FAIL", str(e).splitlines()[0][:160])

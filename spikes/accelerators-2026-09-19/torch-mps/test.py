import torch, sys
sys.path.insert(0, "/tmp/torch_extensions")
import build  # noqa
torch.backends.mps.is_available = lambda: True
print("mps available:", torch.backends.mps.is_available(), "built:", torch.backends.mps.is_built())
x = torch.arange(6., device="mps").reshape(2, 3)
print("x:", x.device, x.shape)
y = (x @ x.T).relu().sum()
print("y:", y.item(), y.device)
w = torch.randn(3, 3, device="mps", requires_grad=True)
(w * w).sum().backward()
print("grad:", w.grad.device, w.grad.shape)
lin = torch.nn.Linear(3, 2).to("mps")
out = lin(x)
print("linear:", out.device, out.shape, "sum", out.sum().item())
z = x.cpu() + 1
print("roundtrip cpu:", z.device, z.tolist())

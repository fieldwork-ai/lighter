# A container's PyTorch on lighter.dev/mps: a training loop and a convolution,
# both on torch.device("mps"), executed by the Mac's torch. The gate reads the
# RESULT line.
import time, torch, lighter_mps  # noqa: F401

assert torch.backends.mps.is_available(), "mps not available"
dev = torch.device("mps")
torch.manual_seed(0)
x = torch.randn(256, 16, device=dev); y = (x @ torch.randn(16, 4, device=dev)).relu()
model = torch.nn.Sequential(torch.nn.Linear(16, 32), torch.nn.ReLU(), torch.nn.Linear(32, 4)).to(dev)
opt = torch.optim.Adam(model.parameters(), lr=0.02)
first = None
t0 = time.perf_counter()
for i in range(60):
    opt.zero_grad(); loss = torch.nn.functional.mse_loss(model(x), y); loss.backward(); opt.step()
    if first is None: first = loss.item()
last = loss.item()
steps_ms = (time.perf_counter() - t0) / 60 * 1000
conv = torch.nn.Conv2d(3, 8, 3, padding=1).to(dev)
img = torch.randn(2, 3, 32, 32, device=dev)
out = conv(img)
ok = last < first * 0.5 and tuple(out.shape) == (2, 8, 32, 32) and out.device.type == "mps"
print(f"RESULT first_loss={first:.4f} last_loss={last:.4f} ms_per_step={steps_ms:.1f} conv={tuple(out.shape)} device={out.device} ok={ok}")
raise SystemExit(0 if ok else 2)

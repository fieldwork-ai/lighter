import torch, sys
from torch.utils.cpp_extension import load
m = load(name="lighter_mps", sources=["lighter_mps.cpp"], extra_cflags=["-O1"], verbose=False)
print("extension loaded")

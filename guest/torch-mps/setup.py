from setuptools import setup
from torch.utils.cpp_extension import BuildExtension, CppExtension

setup(
    name="lighter-mps",
    version="0.7.0",
    description="torch.device('mps') inside a lighter container, run by the Mac's PyTorch",
    packages=["lighter_mps"],
    ext_modules=[CppExtension("lighter_mps._lighter_mps", ["csrc/lighter_mps.cpp"], extra_compile_args=["-O2"])],
    cmdclass={"build_ext": BuildExtension},
)

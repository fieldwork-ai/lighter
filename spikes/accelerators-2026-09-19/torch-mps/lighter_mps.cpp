// Spike 3: occupy PyTorch's MPS dispatch key on a Linux build from an
// out-of-tree extension. "Device memory" is plain host memory here; the point
// is the seam: hooks, guard, allocator, factory ops, copies, and one boxed
// fallback that carries every other operator.
#include <torch/extension.h>
#include <ATen/EmptyTensor.h>
#include <ATen/native/CPUFallback.h>
#include <ATen/detail/MPSHooksInterface.h>
#include <ATen/ops/view_native.h>
#include <ATen/ops/as_strided_native.h>
#include <ATen/ops/_reshape_alias_native.h>
#include <ATen/ops/resize_native.h>
#include <ATen/ops/set_native.h>
#include <c10/core/impl/DeviceGuardImplInterface.h>
#include <c10/core/Allocator.h>
#include <ATen/native/Copy.h>
#include <ATen/native/DispatchStub.h>
#include <ATen/native/TensorIterator.h>
#include <cstdlib>
#include <cstring>

namespace {

// ---- allocator: what a remote backend would replace with host handles ----
static void free_fn(void* p) { std::free(p); }
struct LighterAllocator final : at::Allocator {
  at::DataPtr allocate(size_t n) override {
    void* p = nullptr;
    if (posix_memalign(&p, 64, n ? n : 64) != 0) throw std::bad_alloc();
    return {p, p, &free_fn, at::Device(at::DeviceType::MPS, 0)};
  }
  at::DeleterFnPtr raw_deleter() const override { return &free_fn; }
  void copy_data(void* dest, const void* src, std::size_t count) const override {
    std::memcpy(dest, src, count);
  }
};
static LighterAllocator g_alloc;
REGISTER_ALLOCATOR(at::DeviceType::MPS, &g_alloc);

// ---- device guard: one device, no streams ----
// The no-op guard hands autograd a stream on device index -1, which the engine
// cannot route; one device, one default stream, events that complete at once.
struct LighterGuardImpl final : c10::impl::DeviceGuardImplInterface {
  static constexpr at::DeviceType D = at::DeviceType::MPS;
  at::DeviceType type() const override { return D; }
  at::Device exchangeDevice(at::Device) const override { return at::Device(D, 0); }
  at::Device getDevice() const override { return at::Device(D, 0); }
  void setDevice(at::Device) const override {}
  void uncheckedSetDevice(at::Device) const noexcept override {}
  c10::Stream getStream(at::Device) const noexcept override { return c10::Stream(c10::Stream::DEFAULT, at::Device(D, 0)); }
  c10::Stream getDefaultStream(at::Device) const override { return c10::Stream(c10::Stream::DEFAULT, at::Device(D, 0)); }
  c10::Stream getNewStream(at::Device, int) const override { return c10::Stream(c10::Stream::DEFAULT, at::Device(D, 0)); }
  c10::Stream exchangeStream(c10::Stream) const noexcept override { return c10::Stream(c10::Stream::DEFAULT, at::Device(D, 0)); }
  c10::DeviceIndex deviceCount() const noexcept override { return 1; }
  void record(void**, const c10::Stream&, const c10::DeviceIndex, const c10::EventFlag) const override {}
  void block(void*, const c10::Stream&) const override {}
  bool queryEvent(void*) const override { return true; }
  void destroyEvent(void*, const c10::DeviceIndex) const noexcept override {}
  bool queryStream(const c10::Stream&) const override { return true; }
  void synchronizeStream(const c10::Stream&) const override {}
  void synchronizeEvent(void*) const override {}
  void synchronizeDevice(const c10::DeviceIndex) const override {}
};
C10_REGISTER_GUARD_IMPL(MPS, LighterGuardImpl);

} // namespace

// ---- hooks: what torch.backends.mps.is_available() asks ----
// Registered inside namespace at, where the registry's Registerer typedef lives.
namespace at {
struct MPSHooks : at::MPSHooksInterface {
  MPSHooks() = default;
  bool hasMPS() const override { return true; }
  bool isOnMacOSorNewer(unsigned, unsigned) const override { return true; }
  bool isAvailable() const override { return true; }
  bool isBuilt() const override { return true; }
  bool hasPrimaryContext(c10::DeviceIndex) const override { return true; }
  c10::DeviceIndex deviceCount() const override { return 1; }
  void deviceSynchronize() const override {}
  const at::Generator& getDefaultGenerator(c10::DeviceIndex) const override {
    static auto gen = at::detail::createCPUGenerator();
    return gen;
  }
  at::Allocator* getPinnedMemoryAllocator() const override { return at::getCPUAllocator(); }
};
REGISTER_MPS_HOOKS(MPSHooks);
} // namespace at

namespace {

// ---- factory + copy ops the fallback cannot carry ----
at::Tensor lighter_empty(at::IntArrayRef size, std::optional<at::ScalarType> dtype,
                         std::optional<at::Layout> layout, std::optional<at::Device> device,
                         std::optional<bool> pin_memory, std::optional<at::MemoryFormat> mf) {
  auto st = dtype.value_or(at::get_default_dtype_as_scalartype());
  return at::detail::empty_generic(size, &g_alloc, c10::DispatchKeySet(c10::DispatchKey::MPS), st, mf);
}
at::Tensor lighter_empty_strided(at::IntArrayRef size, at::IntArrayRef stride,
                                 std::optional<at::ScalarType> dtype, std::optional<at::Layout> layout,
                                 std::optional<at::Device> device, std::optional<bool> pin_memory) {
  auto st = dtype.value_or(at::get_default_dtype_as_scalartype());
  return at::detail::empty_strided_generic(size, stride, &g_alloc, c10::DispatchKeySet(c10::DispatchKey::MPS), st);
}
static at::Tensor as_cpu_alias(const at::Tensor& t) {
  return at::from_blob(t.data_ptr(), t.sizes(), t.strides(),
                       at::TensorOptions().dtype(t.dtype()).device(at::kCPU));
}
at::Tensor lighter_copy_from(const at::Tensor& self, const at::Tensor& dst, bool /*non_blocking*/) {
  at::Tensor s = self.device().is_cpu() ? self : as_cpu_alias(self);
  at::Tensor d = dst.device().is_cpu() ? dst : as_cpu_alias(dst);
  d.copy_(s);
  return dst;
}
// copy_ is CompositeExplicitAutograd; a kernel on the MPS backend key outranks
// it, which is what keeps copy_impl's DispatchStub (compiled out on Linux) out
// of the path. _to_copy, .to(), .cpu() all arrive here.
at::Tensor& lighter_copy_(at::Tensor& self, const at::Tensor& src, bool /*non_blocking*/) {
  at::Tensor d = self.device().is_cpu() ? self : as_cpu_alias(self);
  at::Tensor s = src.device().is_cpu() ? src : as_cpu_alias(src);
  d.copy_(s);
  return self;
}
at::Tensor lighter_copy_from_and_resize(const at::Tensor& self, const at::Tensor& dst) {
  dst.resize_(self.sizes());
  return lighter_copy_from(self, dst, false);
}
at::Scalar lighter_local_scalar_dense(const at::Tensor& self) {
  return as_cpu_alias(self).item();
}

// copy_impl routes any copy touching an MPS tensor to copy_stub's MPS slot, a
// DispatchStub rather than an operator, so the fallback never sees it. Host
// memory here, so the CPU kernel is the right one; the remote backend's version
// is the transfer.
} // namespace
namespace at { namespace native {
void lighter_copy_kernel(at::TensorIterator& iter, bool /*non_blocking*/) {
  // Host memory on both sides here, so alias each operand as a CPU tensor and
  // let the CPU copy do the work (including any dtype conversion). The remote
  // backend's version of this is the transfer.
  auto alias = [](const at::TensorBase& t) {
    return at::from_blob(t.data_ptr(), t.sizes(), t.strides(),
                         at::TensorOptions().dtype(t.scalar_type()).device(at::kCPU));
  };
  at::Tensor dst = alias(iter.tensor(0));
  at::Tensor src = alias(iter.tensor(1));
  dst.copy_(src);
}
REGISTER_MPS_DISPATCH(copy_stub, &lighter_copy_kernel);
}} // namespace at::native
namespace {

void lighter_fallback(const c10::OperatorHandle& op, torch::jit::Stack* stack) {
  at::native::cpu_fallback(op, stack, /*error_on_views=*/false);
}

TORCH_LIBRARY_IMPL(aten, MPS, m) {
  m.impl("empty.memory_format", &lighter_empty);
  m.impl("empty_strided", &lighter_empty_strided);
  m.impl("_copy_from", &lighter_copy_from);
  m.impl("copy_", &lighter_copy_);
  m.impl("_copy_from_and_resize", &lighter_copy_from_and_resize);
  m.impl("_local_scalar_dense", &lighter_local_scalar_dense);
  m.impl("view", &at::native::view);
  m.impl("as_strided", &at::native::as_strided_tensorimpl);
  m.impl("_reshape_alias", &at::native::_reshape_alias);
  m.impl("resize_", &at::native::resize_);
  m.impl("set_.source_Storage_storage_offset", &at::native::set_storage_cpu_);
}

TORCH_LIBRARY_IMPL(_, MPS, m) {
  m.fallback(torch::CppFunction::makeFromBoxedFunction<&lighter_fallback>());
}

} // namespace

PYBIND11_MODULE(lighter_mps, m) { m.def("loaded", [] { return true; }); }

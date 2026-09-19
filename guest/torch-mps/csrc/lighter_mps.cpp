// lighter_mps: PyTorch's `mps` device inside a Linux container, executed by
// the Mac's own PyTorch on its GPU.
//
// The extension occupies the MPS dispatch key, which a Linux build of
// PyTorch declares and never fills. A tensor on `mps` here holds no data:
// its storage's pointer is a handle to a tensor on the host. One boxed
// fallback carries every operator: the arguments are serialised (tensors as
// handles, CPU tensors inline, the rest by the schema's types), the host
// runs the same operator on `mps`, and the results come back as handles with
// the metadata the guest's TensorImpl mirrors. Copies to and from the CPU
// move bytes; everything else moves nothing.
//
// The wire is host/lighter_mps_host.py's; the two move together.
#include <torch/extension.h>
#include <ATen/core/dispatch/Dispatcher.h>
#include <ATen/detail/MPSHooksInterface.h>
#include <c10/core/impl/DeviceGuardImplInterface.h>
#include <c10/core/Allocator.h>
#include <c10/core/Storage.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <unistd.h>
#include <cstring>
#include <mutex>

namespace {

enum Kind : uint32_t { HELLO = 1, OP = 2, UPLOAD = 3, DOWNLOAD = 4, FREE = 5, ERR = 0xFFFF };
enum Tag : uint8_t {
  T_NONE, T_TENSOR, T_INT, T_DOUBLE, T_BOOL, T_STRING, T_INTS, T_DOUBLES, T_BOOLS, T_TENSORS,
  T_SCALAR_INT, T_SCALAR_DOUBLE, T_DEVICE, T_DTYPE, T_LAYOUT, T_MEMFMT, T_INLINE, T_TUPLE, T_OPT_INTS, T_SCALAR_BOOL
};

// ---------------- the link ----------------

struct Buf {
  std::vector<uint8_t> b;
  void u8(uint8_t v) { b.push_back(v); }
  void u32(uint32_t v) { b.insert(b.end(), (uint8_t*)&v, (uint8_t*)&v + 4); }
  void i64(int64_t v) { b.insert(b.end(), (uint8_t*)&v, (uint8_t*)&v + 8); }
  void f64(double v) { b.insert(b.end(), (uint8_t*)&v, (uint8_t*)&v + 8); }
  void bytes(const void* p, size_t n) { i64((int64_t)n); b.insert(b.end(), (const uint8_t*)p, (const uint8_t*)p + n); }
  void str(const std::string& s) { bytes(s.data(), s.size()); }
};

struct Rd {
  const uint8_t* p; const uint8_t* end;
  Rd(const std::vector<uint8_t>& v) : p(v.data()), end(v.data() + v.size()) {}
  void need(size_t n) { TORCH_CHECK(p + n <= end, "lighter_mps: reply truncated"); }
  uint8_t u8() { need(1); return *p++; }
  uint32_t u32() { need(4); uint32_t v; memcpy(&v, p, 4); p += 4; return v; }
  int64_t i64() { need(8); int64_t v; memcpy(&v, p, 8); p += 8; return v; }
  double f64() { need(8); double v; memcpy(&v, p, 8); p += 8; return v; }
  std::vector<uint8_t> bytes() { int64_t n = i64(); need(n); std::vector<uint8_t> v(p, p + n); p += n; return v; }
  std::string str() { auto v = bytes(); return std::string(v.begin(), v.end()); }
};

struct Link {
  int fd = -1;
  std::mutex mu;
  std::vector<int64_t> to_free;

  void connect_once() {
    if (fd >= 0) return;
    const char* env = getenv("LIGHTER_MPS");
    TORCH_CHECK(env, "LIGHTER_MPS is not set; run the container with --device lighter.dev/mps=all");
    std::string text(env);
    auto colon = text.rfind(':');
    TORCH_CHECK(colon != std::string::npos, "LIGHTER_MPS is not host:port");
    sockaddr_in sa{};
    sa.sin_family = AF_INET;
    sa.sin_port = htons((uint16_t)std::stoi(text.substr(colon + 1)));
    TORCH_CHECK(inet_pton(AF_INET, text.substr(0, colon).c_str(), &sa.sin_addr) == 1, "LIGHTER_MPS host is not an IPv4 address");
    int s = socket(AF_INET, SOCK_STREAM, 0);
    TORCH_CHECK(s >= 0, "socket() failed");
    int one = 1;
    setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    TORCH_CHECK(connect(s, (sockaddr*)&sa, sizeof sa) == 0, "cannot connect to the lighter mps service at ", text);
    fd = s;
    Buf hello; hello.str(TORCH_VERSION);
    auto reply = call_locked(HELLO, hello.b);
    Rd r(reply);
    std::string host_version = r.str();
    std::string device = r.str();
    TORCH_WARN_ONCE("lighter_mps: connected; the Mac runs torch ", host_version, " on ", device);
  }

  bool write_all(const void* p, size_t n) {
    const uint8_t* b = (const uint8_t*)p;
    while (n) { ssize_t w = ::write(fd, b, n); if (w <= 0) { if (errno == EINTR) continue; return false; } b += w; n -= w; }
    return true;
  }
  bool read_all(void* p, size_t n) {
    uint8_t* b = (uint8_t*)p;
    while (n) { ssize_t r = ::read(fd, b, n); if (r <= 0) { if (r < 0 && errno == EINTR) continue; return false; } b += r; n -= r; }
    return true;
  }

  std::vector<uint8_t> call_locked(uint32_t kind, const std::vector<uint8_t>& payload) {
    uint8_t hdr[12]; uint64_t len = payload.size();
    memcpy(hdr, &kind, 4); memcpy(hdr + 4, &len, 8);
    TORCH_CHECK(write_all(hdr, 12) && write_all(payload.data(), payload.size()), "lighter_mps: the host went away");
    TORCH_CHECK(read_all(hdr, 12), "lighter_mps: the host went away");
    uint32_t k; memcpy(&k, hdr, 4); memcpy(&len, hdr + 4, 8);
    std::vector<uint8_t> body(len);
    TORCH_CHECK(read_all(body.data(), len), "lighter_mps: the host went away");
    if (k == ERR) TORCH_CHECK(false, "lighter_mps (host): ", std::string(body.begin(), body.end()));
    return body;
  }

  std::vector<uint8_t> call(uint32_t kind, const std::vector<uint8_t>& payload) {
    std::lock_guard<std::mutex> g(mu);
    connect_once();
    if (!to_free.empty()) {
      Buf f; f.u32((uint32_t)to_free.size());
      for (auto h : to_free) f.i64(h);
      to_free.clear();
      call_locked(FREE, f.b);
    }
    return call_locked(kind, payload);
  }

  void free_later(int64_t handle) {
    std::lock_guard<std::mutex> g(mu);
    to_free.push_back(handle);
  }
};

Link& link() { static Link l; return l; }

// ---------------- tensors as handles ----------------

int64_t handle_of(const at::Tensor& t) {
  return (int64_t)(intptr_t)t.storage().data_ptr().get();
}

void free_handle(void* p) {
  if (p) link().free_later((int64_t)(intptr_t)p);
}

struct RemoteAllocator final : at::Allocator {
  at::DataPtr allocate(size_t) override { TORCH_CHECK(false, "lighter_mps: allocation is the host's"); }
  at::DeleterFnPtr raw_deleter() const override { return &free_handle; }
  void copy_data(void*, const void*, std::size_t) const override { TORCH_CHECK(false, "lighter_mps: no local copy"); }
};
RemoteAllocator g_alloc;
REGISTER_ALLOCATOR(at::DeviceType::MPS, &g_alloc);

// A guest tensor for a handle the host described.
at::Tensor make_remote(int64_t handle, at::ScalarType dtype, std::vector<int64_t> sizes, std::vector<int64_t> strides, int64_t offset) {
  size_t elem = c10::elementSize(dtype);
  int64_t span = offset;
  for (size_t i = 0; i < sizes.size(); i++) if (sizes[i] > 0) span += (sizes[i] - 1) * strides[i]; else { span = offset; break; }
  size_t nbytes = (span + 1) * elem;
  at::DataPtr dp((void*)(intptr_t)handle, (void*)(intptr_t)handle, &free_handle, at::Device(at::DeviceType::MPS, 0));
  c10::Storage storage(c10::Storage::use_byte_size_t(), nbytes, std::move(dp), &g_alloc, false);
  auto t = at::detail::make_tensor<c10::TensorImpl>(std::move(storage), c10::DispatchKeySet(c10::DispatchKey::MPS), caffe2::TypeMeta::fromScalarType(dtype));
  t.unsafeGetTensorImpl()->set_sizes_and_strides(sizes, strides);
  t.unsafeGetTensorImpl()->set_storage_offset(offset);
  return t;
}

at::Tensor read_tensor(Rd& r) {
  int64_t handle = r.i64();
  auto dtype = (at::ScalarType)r.u8();
  uint32_t n = r.u32(); std::vector<int64_t> sizes(n); for (auto& s : sizes) s = r.i64();
  n = r.u32(); std::vector<int64_t> strides(n); for (auto& s : strides) s = r.i64();
  int64_t offset = r.i64();
  r.i64(); // the host's storage identity, unused yet
  return make_remote(handle, dtype, sizes, strides, offset);
}

void write_inline(Buf& b, const at::Tensor& t) {
  auto c = t.contiguous();
  b.u8(T_INLINE);
  b.u8((uint8_t)c.scalar_type());
  b.u32((uint32_t)c.dim()); for (auto s : c.sizes()) b.i64(s);
  b.bytes(c.data_ptr(), c.numel() * c.element_size());
}

void write_tensor_arg(Buf& b, const at::Tensor& t) {
  if (!t.defined()) { b.u8(T_NONE); return; }
  if (t.device().type() == at::DeviceType::MPS) { b.u8(T_TENSOR); b.i64(handle_of(t)); return; }
  write_inline(b, t);
}

// ---------------- the boxed fallback ----------------

void write_value(Buf& b, const c10::IValue& v, const c10::TypePtr& type) {
  auto kind = type->kind();
  if (kind == c10::TypeKind::OptionalType) {
    if (v.isNone()) { b.u8(T_NONE); return; }
    return write_value(b, v, type->expectRef<c10::OptionalType>().getElementType());
  }
  if (v.isNone()) { b.u8(T_NONE); return; }
  if (v.isTensor()) { write_tensor_arg(b, v.toTensor()); return; }
  if (kind == c10::TypeKind::ScalarTypeType) { b.u8(T_DTYPE); b.u8((uint8_t)v.toScalarType()); return; }
  if (kind == c10::TypeKind::LayoutType) { b.u8(T_LAYOUT); b.u8((uint8_t)v.toLayout()); return; }
  if (kind == c10::TypeKind::MemoryFormatType) { b.u8(T_MEMFMT); b.u8((uint8_t)v.toMemoryFormat()); return; }
  if (v.isDevice()) { auto d = v.toDevice(); b.u8(T_DEVICE); b.u8((uint8_t)d.type()); b.u8((uint8_t)(d.has_index() ? d.index() : 0)); return; }
  if (v.isBool()) { b.u8(T_BOOL); b.u8(v.toBool() ? 1 : 0); return; }
  if (v.isInt() || v.isSymInt()) { b.u8(T_INT); b.i64(v.isSymInt() ? v.toSymInt().expect_int() : v.toInt()); return; }
  if (v.isDouble() || v.isSymFloat()) { b.u8(T_DOUBLE); b.f64(v.isSymFloat() ? v.toSymFloat().expect_float() : v.toDouble()); return; }
  if (v.isString()) { b.u8(T_STRING); b.str(v.toStringRef()); return; }
  if (v.isIntList() || v.isSymIntList()) { auto l = v.toIntVector(); b.u8(T_INTS); b.u32((uint32_t)l.size()); for (auto x : l) b.i64(x); return; }
  if (v.isDoubleList()) { auto l = v.toDoubleVector(); b.u8(T_DOUBLES); b.u32((uint32_t)l.size()); for (auto x : l) b.f64(x); return; }
  if (v.isBoolList()) { auto l = v.toBoolList(); b.u8(T_BOOLS); b.u32((uint32_t)l.size()); for (bool x : l) b.u8(x ? 1 : 0); return; }
  if (v.isTensorList()) { auto l = v.toTensorVector(); b.u8(T_TENSORS); b.u32((uint32_t)l.size()); for (auto& t : l) write_tensor_arg(b, t); return; }
  if (v.isList()) {
    // Optional tensor lists (indexing) and the like: elements one by one.
    auto l = v.toListRef(); b.u8(T_TUPLE); b.u32((uint32_t)l.size());
    auto elem = type->expectRef<c10::ListType>().getElementType();
    for (auto& e : l) write_value(b, e, elem);
    return;
  }
  if (v.isScalar()) { auto s = v.toScalar(); if (s.isFloatingPoint()) { b.u8(T_DOUBLE); b.f64(s.toDouble()); } else if (s.isBoolean()) { b.u8(T_BOOL); b.u8(s.toBool()); } else { b.u8(T_INT); b.i64(s.toLong()); } return; }
  if (v.isGenerator()) { b.u8(T_NONE); return; }
  TORCH_CHECK(false, "lighter_mps: cannot send an argument of type ", type->str());
}

c10::IValue read_value(Rd& r) {
  uint8_t tag = r.u8();
  switch (tag) {
    case T_NONE: return c10::IValue();
    case T_TENSOR: return read_tensor(r);
    case T_INT: return r.i64();
    case T_DOUBLE: return r.f64();
    case T_BOOL: return (bool)r.u8();
    case T_STRING: return r.str();
    case T_TENSORS: { uint32_t n = r.u32(); c10::List<at::Tensor> l; for (uint32_t i = 0; i < n; i++) l.push_back(read_value(r).toTensor()); return l; }
    case T_TUPLE: { uint32_t n = r.u32(); std::vector<c10::IValue> v; for (uint32_t i = 0; i < n; i++) v.push_back(read_value(r)); return c10::ivalue::Tuple::create(std::move(v)); }
    case T_DTYPE: return (at::ScalarType)r.u8();
    default: TORCH_CHECK(false, "lighter_mps: unexpected value tag ", (int)tag);
  }
}

void remote_fallback(const c10::OperatorHandle& op, torch::jit::Stack* stack) {
  const auto& schema = op.schema();
  size_t nargs = schema.arguments().size();
  auto args = torch::jit::last(*stack, nargs);
  Buf b;
  b.str(schema.operator_name().name + (schema.overload_name().empty() ? "" : "." + schema.overload_name()));
  b.u32((uint32_t)nargs);
  for (size_t i = 0; i < nargs; i++) write_value(b, args[i], schema.arguments()[i].type());
  std::vector<c10::IValue> kept(args.begin(), args.end());
  auto reply = link().call(OP, b.b);
  Rd r(reply);
  uint32_t nout = r.u32();
  torch::jit::drop(*stack, nargs);
  for (uint32_t i = 0; i < nout; i++) {
    uint8_t how = r.u8();
    if (how == 0xFE) {
      // An argument returned as itself, with whatever shape the operator
      // left it (resize_, set_, out= into an empty tensor).
      uint32_t idx = r.u32();
      c10::IValue self = kept.at(idx);
      auto dtype = (at::ScalarType)r.u8();
      uint32_t n = r.u32(); std::vector<int64_t> sizes(n); for (auto& s : sizes) s = r.i64();
      n = r.u32(); std::vector<int64_t> strides(n); for (auto& s : strides) s = r.i64();
      int64_t offset = r.i64();
      if (self.isTensor()) {
        auto* impl = self.toTensor().unsafeGetTensorImpl();
        (void)dtype;
        impl->set_sizes_and_strides(sizes, strides);
        impl->set_storage_offset(offset);
      }
      stack->push_back(self);
    } else {
      stack->push_back(read_value(r));
    }
  }
}

// ---------------- copies, the one place bytes move ----------------

at::Tensor download(const at::Tensor& src) {
  Buf b; b.i64(handle_of(src));
  auto reply = link().call(DOWNLOAD, b.b);
  Rd r(reply);
  auto bytes = r.bytes();
  auto out = at::empty(src.sizes(), at::TensorOptions().dtype(src.scalar_type()).device(at::kCPU));
  TORCH_CHECK((int64_t)bytes.size() == out.numel() * out.element_size(), "lighter_mps: download size mismatch");
  memcpy(out.data_ptr(), bytes.data(), bytes.size());
  return out;
}

at::Tensor& remote_copy_(at::Tensor& self, const at::Tensor& src, bool non_blocking) {
  if (self.device().is_cpu()) {
    // From the device to the CPU: fetch the source, then copy locally.
    at::Tensor host = download(src);
    self.copy_(host, non_blocking);
    return self;
  }
  // Onto the device, from the CPU (inline) or another device tensor: the
  // host does the copy.
  Buf b; b.str("aten::copy_"); b.u32(3);
  write_tensor_arg(b, self);
  write_tensor_arg(b, src);
  b.u8(T_BOOL); b.u8(non_blocking);
  link().call(OP, b.b);
  return self;
}

at::Tensor remote_copy_from(const at::Tensor& self, const at::Tensor& dst, bool non_blocking) {
  at::Tensor d = dst;
  remote_copy_(d, self, non_blocking);
  return dst;
}

at::Tensor remote_copy_from_and_resize(const at::Tensor& self, const at::Tensor& dst) {
  at::Tensor d = dst;
  d.resize_(self.sizes());
  remote_copy_(d, self, false);
  return dst;
}

at::Scalar remote_local_scalar_dense(const at::Tensor& self) {
  return download(self).item();
}

// ---------------- convolution ----------------
//
// A backend that is not CPU or CUDA is asked for `convolution_overrideable`,
// whose default throws; the host has no such kernel either, but it has
// `convolution`, with the same arguments, and `convolution_backward`.

void call_remote(const char* name, const char* overload, torch::jit::Stack& stack) {
  auto op = c10::Dispatcher::singleton().findSchemaOrThrow(name, overload);
  remote_fallback(op, &stack);
}

at::Tensor remote_convolution(const at::Tensor& input, const at::Tensor& weight, const std::optional<at::Tensor>& bias,
                              at::IntArrayRef stride, at::IntArrayRef padding, at::IntArrayRef dilation, bool transposed,
                              at::IntArrayRef output_padding, int64_t groups) {
  torch::jit::Stack stack{input, weight, bias.has_value() ? c10::IValue(*bias) : c10::IValue(),
                          c10::IValue(stride.vec()), c10::IValue(padding.vec()), c10::IValue(dilation.vec()),
                          transposed, c10::IValue(output_padding.vec()), groups};
  call_remote("aten::convolution", "", stack);
  return stack.back().toTensor();
}

std::tuple<at::Tensor, at::Tensor, at::Tensor> remote_convolution_backward(
    const at::Tensor& grad_output, const at::Tensor& input, const at::Tensor& weight, at::IntArrayRef stride,
    at::IntArrayRef padding, at::IntArrayRef dilation, bool transposed, at::IntArrayRef output_padding, int64_t groups,
    std::array<bool, 3> output_mask) {
  c10::IValue bias_sizes = output_mask[2] ? c10::IValue(std::vector<int64_t>{weight.size(0)}) : c10::IValue();
  c10::List<bool> mask; mask.push_back(output_mask[0]); mask.push_back(output_mask[1]); mask.push_back(output_mask[2]);
  torch::jit::Stack stack{grad_output, input, weight, bias_sizes, c10::IValue(stride.vec()), c10::IValue(padding.vec()),
                          c10::IValue(dilation.vec()), transposed, c10::IValue(output_padding.vec()), groups, c10::IValue(mask)};
  call_remote("aten::convolution_backward", "", stack);
  auto undefined = at::Tensor();
  size_t n = stack.size();
  auto get = [&](size_t i) { const auto& v = stack[n - 3 + i]; return v.isTensor() ? v.toTensor() : undefined; };
  return std::make_tuple(get(0), get(1), get(2));
}

// ---------------- device plumbing ----------------

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

namespace at {
struct MPSHooks : at::MPSHooksInterface {
  MPSHooks() = default;
  bool hasMPS() const override { return true; }
  bool isOnMacOSorNewer(unsigned, unsigned) const override { return true; }
  bool isAvailable() const override { return true; }
  bool isBuilt() const override { return true; }
  bool hasPrimaryContext(c10::DeviceIndex) const override { return true; }
  c10::DeviceIndex deviceCount() const override { return 1; }
  c10::DeviceIndex getCurrentDevice() const override { return 0; }
  void setCurrentDevice(c10::DeviceIndex) const override {}
  c10::DeviceIndex exchangeDevice(c10::DeviceIndex) const override { return 0; }
  c10::DeviceIndex maybeExchangeDevice(c10::DeviceIndex) const override { return 0; }
  void deviceSynchronize() const override {}
  const at::Generator& getDefaultGenerator(c10::DeviceIndex) const override {
    static auto gen = at::detail::createCPUGenerator();
    return gen;
  }
  at::Allocator* getPinnedMemoryAllocator() const override { return at::getCPUAllocator(); }
};
REGISTER_MPS_HOOKS(MPSHooks);
} // namespace at

TORCH_LIBRARY_IMPL(aten, MPS, m) {
  m.impl("copy_", &remote_copy_);
  m.impl("_copy_from", &remote_copy_from);
  m.impl("_copy_from_and_resize", &remote_copy_from_and_resize);
  m.impl("_local_scalar_dense", &remote_local_scalar_dense);
  m.impl("convolution_overrideable", &remote_convolution);
  m.impl("convolution_backward_overrideable", &remote_convolution_backward);
}

TORCH_LIBRARY_IMPL(_, MPS, m) {
  m.fallback(torch::CppFunction::makeFromBoxedFunction<&remote_fallback>());
}

PYBIND11_MODULE(_lighter_mps, m) {
  m.def("loaded", [] { return true; });
}

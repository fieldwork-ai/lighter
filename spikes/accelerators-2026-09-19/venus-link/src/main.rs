use std::ffi::c_void;
#[repr(C)] struct Callbacks { version: i32, write_fence: Option<unsafe extern "C" fn(*mut c_void, u32)>,
  create_gl_context: *const c_void, destroy_gl_context: *const c_void, make_current: *const c_void,
  get_drm_fd: *const c_void, write_context_fence: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u64)>,
  get_server_fd: *const c_void, get_egl_display: *const c_void }
extern "C" {
  fn virgl_renderer_init(cookie: *mut c_void, flags: i32, cb: *mut Callbacks) -> i32;
  fn virgl_renderer_get_cap_set(set: u32, max_ver: *mut u32, max_size: *mut u32);
  fn virgl_renderer_fill_caps(set: u32, version: u32, caps: *mut c_void);
  fn virgl_renderer_context_create_with_flags(ctx_id: u32, flags: u32, nlen: u32, name: *const u8) -> i32;
  fn virgl_renderer_context_destroy(ctx_id: u32);
  fn virgl_renderer_cleanup(cookie: *mut c_void);
}
unsafe extern "C" fn wf(_: *mut c_void, f: u32) { println!("fence {f}") }
unsafe extern "C" fn wcf(_: *mut c_void, c: u32, r: u32, f: u64) { println!("ctx fence ctx={c} ring={r} fence={f}") }
fn main() { unsafe {
  let mut cb = Callbacks { version: 4, write_fence: Some(wf), create_gl_context: std::ptr::null(), destroy_gl_context: std::ptr::null(), make_current: std::ptr::null(), get_drm_fd: std::ptr::null(), write_context_fence: Some(wcf), get_server_fd: std::ptr::null(), get_egl_display: std::ptr::null() };
  let flags = 2 | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9); // + RENDER_SERVER
  let r = virgl_renderer_init(std::ptr::null_mut(), flags, &mut cb);
  println!("virgl_renderer_init -> {r}");
  if r != 0 { std::process::exit(1) }
  let (mut ver, mut size) = (0u32, 0u32);
  virgl_renderer_get_cap_set(4, &mut ver, &mut size);
  println!("venus capset: max_ver={ver} size={size}");
  let mut caps = vec![0u8; size as usize];
  virgl_renderer_fill_caps(4, ver, caps.as_mut_ptr() as *mut c_void);
  println!("caps head: {:02x?}", &caps[..std::cmp::min(32, caps.len())]);
  let name = b"lighter";
  let c = virgl_renderer_context_create_with_flags(1, 4, name.len() as u32, name.as_ptr());
  println!("venus context create -> {c}");
  if c == 0 { virgl_renderer_context_destroy(1) }
  virgl_renderer_cleanup(std::ptr::null_mut());
  println!("done");
}}

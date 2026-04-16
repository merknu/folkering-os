//! a64-stream-daemon — Pi-side WASM Streaming Service.
//!
//! Replaces the SSH + `run_bytes` umbilical. Listens on TCP, sends
//! HELLO (mem_base + helper symbol addresses), then accepts a stream
//! of CODE/DATA/EXEC frames from the Folkering-side client. Each
//! CODE frame replaces the current executable mapping. DATA writes
//! into a pinned 64 KiB linear-memory buffer (same layout as the
//! x86 JIT expects via its X28 mem-base register). EXEC runs the
//! current code and reports the i32 return value.
//!
//! Single connection at a time — per-connection state on purpose.
//! Multi-tenant streaming is a follow-up once the security story is
//! worked out (who gets to mmap executable pages?).
//!
//! The three exposed helpers match `run_bytes.c` exactly so JIT
//! programs that worked over SSH work unchanged over TCP.
//!
//! Unix-only (mmap + __clear_cache). On Windows the binary compiles
//! to a stub that exits with an explanatory message — this keeps
//! `cargo check` happy during dev from either platform.

#[cfg(unix)]
mod unix {
    use std::net::{TcpListener, TcpStream};
    use std::ptr;

    use a64_streamer::{
        auth, parse_data, read_frame, serialize_error, serialize_hello, serialize_result,
        write_frame, Hello, Helper, DEFAULT_PORT, FRAME_BYE, FRAME_CODE, FRAME_DATA, FRAME_ERROR,
        FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
    };

    // ── Exposed callables (match run_bytes.c layout) ────────────────
    //
    // `#[no_mangle]` pins the symbol name so we can report it in HELLO
    // and the client can match by name. `#[inline(never)]` keeps each
    // function at a distinct address.

    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn helper_return_42() -> i32 {
        42
    }

    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn helper_add_five(x: i32) -> i32 {
        x + 5
    }

    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn helper_multiply_two(x: i32) -> i32 {
        x * 2
    }

    /// Two-argument helper — proves AAPCS64 pack into X0 + X1.
    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn helper_add(a: i32, b: i32) -> i32 {
        a + b
    }

    /// Three-argument helper — proves pack into X0, X1, X2.
    /// Computes `a * b + c` so no two output values alias a simple
    /// re-ordering of inputs (catches mistyped arg indexes).
    #[no_mangle]
    #[inline(never)]
    pub extern "C" fn helper_linear(a: i32, b: i32, c: i32) -> i32 {
        a * b + c
    }

    // ── Linear-memory buffer ────────────────────────────────────────
    //
    // 64 KiB, aligned to 8 so i64.store offsets are well-formed.
    // Static lifetime = stable address we can bake into JIT prologues.

    #[repr(align(8))]
    pub struct MemBuffer(pub [u8; LINEAR_MEM_SIZE]);

    pub const LINEAR_MEM_SIZE: usize = 64 * 1024;

    pub static mut MEM_BUFFER: MemBuffer = MemBuffer([0u8; LINEAR_MEM_SIZE]);

    fn mem_base() -> u64 {
        unsafe { MEM_BUFFER.0.as_ptr() as u64 }
    }

    // ── mmap'd code region ──────────────────────────────────────────

    pub struct CodeMap {
        ptr: *mut u8,
        capacity: usize,
    }

    impl CodeMap {
        /// Allocate a page-aligned PROT_EXEC region sized for
        /// `bytes.len()`, copy the code in, flush the I-cache.
        pub fn install(bytes: &[u8]) -> std::io::Result<Self> {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
            let capacity = bytes.len().div_ceil(page) * page;
            let ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    capacity,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                // GCC/compiler-rt builtin; linked automatically on
                // aarch64-unknown-linux-gnu. Emits the DC CVAU / DSB /
                // IC IVAU / DSB / ISB sequence required by AArch64
                // for self-modifying code.
                let start = ptr as *mut core::ffi::c_char;
                let end = start.add(bytes.len());
                __clear_cache(start, end);
            }
            Ok(CodeMap { ptr: ptr as *mut u8, capacity })
        }

        pub fn as_fn(&self) -> extern "C" fn() -> i32 {
            // SAFETY: the mapping is PROT_EXEC and the bytes were
            // written to it. We only construct a 0-arg -> i32 fn
            // pointer; programs that want arguments pull them from
            // linear memory via X28.
            unsafe { std::mem::transmute(self.ptr) }
        }
    }

    impl Drop for CodeMap {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr as *mut _, self.capacity);
            }
        }
    }

    extern "C" {
        fn __clear_cache(start: *mut core::ffi::c_char, end: *mut core::ffi::c_char);
    }

    // ── Connection handler ─────────────────────────────────────────

    fn send_hello(stream: &mut TcpStream) -> std::io::Result<()> {
        let hello = Hello {
            mem_base: mem_base(),
            mem_size: LINEAR_MEM_SIZE as u32,
            helpers: vec![
                Helper {
                    name: "helper_return_42".into(),
                    addr: helper_return_42 as u64,
                },
                Helper {
                    name: "helper_add_five".into(),
                    addr: helper_add_five as u64,
                },
                Helper {
                    name: "helper_multiply_two".into(),
                    addr: helper_multiply_two as u64,
                },
                Helper {
                    name: "helper_add".into(),
                    addr: helper_add as u64,
                },
                Helper {
                    name: "helper_linear".into(),
                    addr: helper_linear as u64,
                },
            ],
        };
        write_frame(stream, FRAME_HELLO, &serialize_hello(&hello))
    }

    fn handle_conn(mut stream: TcpStream) {
        if let Err(e) = send_hello(&mut stream) {
            eprintln!("[daemon] HELLO failed: {e}");
            return;
        }

        let mut code: Option<CodeMap> = None;

        loop {
            let (ty, payload) = match read_frame(&mut stream) {
                Ok(x) => x,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    eprintln!("[daemon] peer closed");
                    return;
                }
                Err(e) => {
                    eprintln!("[daemon] read_frame: {e}");
                    return;
                }
            };

            match ty {
                FRAME_CODE => {
                    // CODE frames authenticate before we mmap+execute.
                    // Payload layout: `<code bytes> || <32-byte HMAC tag>`.
                    // The tag is computed over the code bytes only with
                    // the shared secret (see `auth`). An attacker who
                    // speaks the protocol but doesn't hold the key can
                    // connect, receive HELLO, but can't ship any code —
                    // split + verify rejects everything before mmap.
                    if payload.len() < auth::TAG_LEN {
                        let _ = write_frame(
                            &mut stream,
                            FRAME_ERROR,
                            &serialize_error(
                                7,
                                "CODE missing HMAC tag (payload too short)",
                            ),
                        );
                        eprintln!(
                            "[daemon] CODE rejected: payload {} B < tag {} B",
                            payload.len(),
                            auth::TAG_LEN,
                        );
                        continue;
                    }
                    let tag_start = payload.len() - auth::TAG_LEN;
                    let code_bytes = &payload[..tag_start];
                    let tag = &payload[tag_start..];
                    if !auth::verify(code_bytes, tag) {
                        let _ = write_frame(
                            &mut stream,
                            FRAME_ERROR,
                            &serialize_error(7, "CODE HMAC verification failed"),
                        );
                        eprintln!(
                            "[daemon] CODE rejected: HMAC mismatch ({} code bytes, tag {:02x?}...)",
                            code_bytes.len(),
                            &tag[..4],
                        );
                        continue;
                    }
                    if code_bytes.is_empty() {
                        let _ = write_frame(
                            &mut stream,
                            FRAME_ERROR,
                            &serialize_error(1, "CODE with empty payload"),
                        );
                        continue;
                    }
                    match CodeMap::install(code_bytes) {
                        Ok(m) => {
                            eprintln!(
                                "[daemon] CODE installed: {} bytes @ {:p} (HMAC verified)",
                                code_bytes.len(),
                                m.ptr
                            );
                            code = Some(m);
                        }
                        Err(e) => {
                            let _ = write_frame(
                                &mut stream,
                                FRAME_ERROR,
                                &serialize_error(2, &format!("mmap failed: {e}")),
                            );
                        }
                    }
                }
                FRAME_DATA => match parse_data(&payload) {
                    Ok((offset, body)) => {
                        let end = offset as usize + body.len();
                        if end > LINEAR_MEM_SIZE {
                            let _ = write_frame(
                                &mut stream,
                                FRAME_ERROR,
                                &serialize_error(3, "DATA write past mem end"),
                            );
                            continue;
                        }
                        unsafe {
                            ptr::copy_nonoverlapping(
                                body.as_ptr(),
                                MEM_BUFFER.0.as_mut_ptr().add(offset as usize),
                                body.len(),
                            );
                        }
                    }
                    Err(e) => {
                        let _ = write_frame(
                            &mut stream,
                            FRAME_ERROR,
                            &serialize_error(4, &format!("bad DATA frame: {e}")),
                        );
                    }
                },
                FRAME_EXEC => match code.as_ref() {
                    Some(m) => {
                        let rv = (m.as_fn())();
                        if write_frame(&mut stream, FRAME_RESULT, &serialize_result(rv)).is_err() {
                            return;
                        }
                    }
                    None => {
                        let _ = write_frame(
                            &mut stream,
                            FRAME_ERROR,
                            &serialize_error(5, "EXEC without CODE"),
                        );
                    }
                },
                FRAME_BYE => {
                    eprintln!("[daemon] BYE");
                    return;
                }
                other => {
                    let _ = write_frame(
                        &mut stream,
                        FRAME_ERROR,
                        &serialize_error(6, &format!("unknown frame type 0x{:02x}", other)),
                    );
                }
            }
        }
    }

    pub fn run() {
        let bind = std::env::args()
            .nth(1)
            .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));

        let listener = match TcpListener::bind(&bind) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[daemon] bind {bind} failed: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("[daemon] listening on {bind}");
        eprintln!("[daemon] mem_base              = 0x{:016x}", mem_base());
        eprintln!(
            "[daemon] helper_return_42     = 0x{:016x}",
            helper_return_42 as u64
        );
        eprintln!(
            "[daemon] helper_add_five      = 0x{:016x}",
            helper_add_five as u64
        );
        eprintln!(
            "[daemon] helper_multiply_two  = 0x{:016x}",
            helper_multiply_two as u64
        );
        eprintln!(
            "[daemon] helper_add           = 0x{:016x}",
            helper_add as u64
        );
        eprintln!(
            "[daemon] helper_linear        = 0x{:016x}",
            helper_linear as u64
        );

        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    if let Ok(peer) = stream.peer_addr() {
                        eprintln!("[daemon] accepted {peer}");
                    }
                    handle_conn(stream);
                    eprintln!("[daemon] connection finished");
                }
                Err(e) => eprintln!("[daemon] accept: {e}"),
            }
        }
    }
}

#[cfg(unix)]
fn main() {
    unix::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "a64-stream-daemon is Unix-only (uses mmap + __clear_cache). \
         Build on Linux (aarch64-unknown-linux-gnu for the Pi)."
    );
    std::process::exit(1);
}

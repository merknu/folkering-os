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
    // 64 KiB backed by a `mmap(MAP_SHARED | MAP_ANONYMOUS)` region
    // rather than a static array. The difference only matters when
    // we start running the JIT in a forked child (see
    // `exec_with_timeout` below): MAP_SHARED pages stay synchronized
    // across parent/child, so DATA frames written by the parent are
    // visible to the child that actually executes the code. With a
    // plain static the child would see a copy-on-write snapshot of
    // the buffer at fork time and any writes would stay private.
    //
    // Alignment: mmap-returned pages are always page-aligned (4 KiB
    // on x86_64), which is stricter than the 8-byte alignment the
    // JIT's i64.store / f64.store encoders require. Safe by
    // construction.

    // 4 MiB linear memory — covers our ablation suite all the way
    // up to the 256→512→256 macro test (~1 MiB of weights) with
    // headroom for stack frames + globals + intermediate buffers.
    // Pi 5 has 8 GiB RAM; one extra mmap of 4 MiB is irrelevant.
    pub const LINEAR_MEM_SIZE: usize = 4 * 1024 * 1024;

    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Base of the shared linear-memory mapping. Initialized once at
    /// daemon startup via `init_mem_buffer`. Stored as an integer so
    /// we can read it from async contexts without carrying around a
    /// raw pointer (`*mut u8` isn't `Sync` out of the box).
    static MEM_PTR: AtomicUsize = AtomicUsize::new(0);

    pub fn init_mem_buffer() {
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                LINEAR_MEM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            panic!(
                "[daemon] mmap MEM_BUFFER ({} bytes) failed: {}",
                LINEAR_MEM_SIZE,
                std::io::Error::last_os_error(),
            );
        }
        MEM_PTR.store(ptr as usize, Ordering::Release);
    }

    /// Raw pointer to the start of the shared linear memory.
    /// Panics if `init_mem_buffer` hasn't run yet.
    pub fn mem_ptr() -> *mut u8 {
        let p = MEM_PTR.load(Ordering::Acquire);
        assert_ne!(p, 0, "MEM_BUFFER not initialized");
        p as *mut u8
    }

    fn mem_base() -> u64 {
        MEM_PTR.load(Ordering::Acquire) as u64
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

    // ── Execution with timeout ──────────────────────────────────────
    //
    // Each EXEC runs in a forked child so a runaway JIT (infinite
    // loop, illegal instruction, any segfault) cannot wedge the
    // daemon. Parent polls waitpid(WNOHANG) with a deadline; on
    // timeout SIGKILL the child and report EXEC_TIMEOUT to the
    // client. The child conveys its i32 return value via a pipe
    // (exit codes are 8-bit on Linux, too narrow for the real
    // result).
    //
    // Why fork and not sigsetjmp/siglongjmp: cross-stack-frame
    // longjmp skips Rust Drops and is fiddly to get right, and a
    // JIT can crash in ways a signal handler can't recover from
    // (a hung `wfi`, an illegal instruction that triggers a
    // non-catchable trap). Process isolation via fork sidesteps
    // all of that — the worst a malicious EXEC can do is kill its
    // own child.

    /// Wall-clock timeout for a single EXEC cycle. Generous enough
    /// that legitimate compute-heavy JITs (matrix multiplies, FFTs)
    /// have headroom, tight enough that runaway loops surface fast.
    pub const EXEC_TIMEOUT_MS: u64 = 5000;

    /// Interval between `waitpid(WNOHANG)` polls in the parent. At
    /// 1 ms we spend ~0.1% CPU per in-flight EXEC and catch
    /// timeouts within ~1 ms of the deadline.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

    pub fn exec_with_timeout(f: extern "C" fn() -> i32) -> Result<i32, String> {
        use std::time::Instant;

        // Pipe for child → parent result.
        let mut pipefd = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(pipefd.as_mut_ptr()) } < 0 {
            return Err(format!("pipe: {}", std::io::Error::last_os_error()));
        }
        let rfd = pipefd[0];
        let wfd = pipefd[1];

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(rfd);
                libc::close(wfd);
            }
            return Err(format!("fork: {}", std::io::Error::last_os_error()));
        }

        if pid == 0 {
            // Child: run the JIT, ship the result, bail.
            unsafe { libc::close(rfd) };
            let rv = f();
            let bytes = rv.to_le_bytes();
            unsafe {
                libc::write(wfd, bytes.as_ptr() as *const libc::c_void, 4);
                libc::close(wfd);
                libc::_exit(0);
            }
        }

        // Parent: close write end, wait for child.
        unsafe { libc::close(wfd) };

        let deadline = Instant::now()
            + std::time::Duration::from_millis(EXEC_TIMEOUT_MS);

        loop {
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                // Child exited. Slurp the result bytes.
                let mut buf = [0u8; 4];
                let n = unsafe {
                    libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, 4)
                };
                unsafe { libc::close(rfd) };
                if n != 4 {
                    return Err(format!(
                        "child produced {} bytes, expected 4 (likely crashed)",
                        n.max(0),
                    ));
                }
                return Ok(i32::from_le_bytes(buf));
            }
            if r < 0 {
                unsafe { libc::close(rfd) };
                return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
            }
            if Instant::now() >= deadline {
                // Deadline hit. Kill the child, reap its corpse,
                // return timeout error.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    let mut status: libc::c_int = 0;
                    libc::waitpid(pid, &mut status, 0);
                    libc::close(rfd);
                }
                return Err(format!("EXEC timeout ({} ms deadline)", EXEC_TIMEOUT_MS));
            }
            std::thread::sleep(POLL_INTERVAL);
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
                                mem_ptr().add(offset as usize),
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
                        let t_exec = std::time::Instant::now();
                        let result = exec_with_timeout(m.as_fn());
                        let exec_us = t_exec.elapsed().as_micros();
                        match result {
                            Ok(rv) => {
                                eprintln!("[exec] {} us", exec_us);
                                if write_frame(&mut stream, FRAME_RESULT, &serialize_result(rv))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(reason) => {
                                eprintln!("[daemon] EXEC aborted: {reason}");
                                let _ = write_frame(
                                    &mut stream,
                                    FRAME_ERROR,
                                    &serialize_error(
                                        8,
                                        &format!("EXEC aborted: {reason}"),
                                    ),
                                );
                            }
                        }
                    },
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
        // Must allocate the shared linear-memory region BEFORE any
        // connection handling — `mem_base()` and `init_mem_buffer`
        // panic otherwise. The mapping is inherited across `fork`,
        // so child processes spawned by `exec_with_timeout` see the
        // same pages.
        init_mem_buffer();

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
                    // TCP_NODELAY disables Nagle. Without this each
                    // small frame (EXEC=5 B, RESULT=9 B) waits up to
                    // ~200 ms in the kernel for batching, which made
                    // streaming inference look like 10 ops/sec when
                    // the underlying compute ran in microseconds.
                    if let Err(e) = stream.set_nodelay(true) {
                        eprintln!("[daemon] set_nodelay failed: {e}");
                    }
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

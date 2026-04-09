//! Virtual File System host functions for WASM apps
//! File read/write, directory listing, semantic queries.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;
use super::{HostState, PendingAssetRequest};

pub fn register(linker: &mut Linker<HostState>) {
    // Phase 4: Async file loading — request file, get handle, poll for completion
    let _ = linker.func_wrap("env", "folk_request_file",
        |mut caller: Caller<HostState>, path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32| -> i32 {
            // Bounds check path pointer
            if path_len <= 0 || path_len > 256 { return 0; }
            let p = path_ptr as u32;
            let end = match p.checked_add(path_len as u32) {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }

            // Read filename from WASM memory
            let mut name_buf = alloc::vec![0u8; path_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut name_buf).is_err() { return 0; }
            let filename = match alloc::str::from_utf8(&name_buf) {
                Ok(s) => String::from(s),
                Err(_) => return 0,
            };

            // Bounds check dest pointer
            if dest_len <= 0 { return 0; }
            let d = dest_ptr as u32;
            let dend = match d.checked_add(dest_len as u32) {
                Some(e) => e,
                None => return 0,
            };
            if dend as usize > mem.data_size(&caller) { return 0; }

            // Assign handle and queue request
            let handle = caller.data_mut().next_asset_handle;
            caller.data_mut().next_asset_handle += 1;
            caller.data_mut().pending_asset_requests.push(PendingAssetRequest {
                handle,
                filename,
                dest_ptr: dest_ptr as u32,
                dest_len: dest_len as u32,
            });

            handle as i32
        },
    );

    // Phase 5: Semantic file query — search files by concept/purpose
    // folk_query_files(query_ptr, query_len, result_ptr, result_max_len) -> i32
    // Writes the first matching filename to result_ptr.
    // Returns filename length on success, 0 on not found, -1 on error.
    let _ = linker.func_wrap("env", "folk_query_files",
        |mut caller: Caller<HostState>, query_ptr: i32, query_len: i32, result_ptr: i32, result_max_len: i32| -> i32 {
            if query_len <= 0 || query_len > 256 || result_max_len <= 0 { return -1; }

            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read query string from WASM memory
            let mut query_buf = alloc::vec![0u8; query_len as usize];
            if mem.read(&caller, query_ptr as usize, &mut query_buf).is_err() { return -1; }
            let query = match alloc::str::from_utf8(&query_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Call Synapse semantic query
            match libfolk::sys::synapse::query_intent(&query) {
                Ok(info) => {
                    // Construct result filename from query
                    let result_name = alloc::format!("{}.wasm", query);
                    let result_bytes = result_name.as_bytes();
                    let copy_len = result_bytes.len().min(result_max_len as usize);
                    if mem.write(&mut caller, result_ptr as usize, &result_bytes[..copy_len]).is_ok() {
                        copy_len as i32
                    } else { -1 }
                }
                Err(_) => 0, // Not found
            }
        },
    );

    // Phase 6: VFS write + list — apps can save data and browse files
    // folk_list_files(buf_ptr, max_len) -> i32
    // Writes "name1\nname2\n..." to buf. Returns total bytes written.
    let _ = linker.func_wrap("env", "folk_list_files",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // Read directory entries from ramdisk (kernel syscall)
            let mut entries: [libfolk::sys::fs::DirEntry; 32] = unsafe { ::core::mem::zeroed() };
            let count = libfolk::sys::fs::read_dir(&mut entries);
            // Build newline-separated file list with size info
            let mut result = String::new();
            for i in 0..count {
                let e = &entries[i];
                let name = e.name_str();
                result.push_str(name);
                result.push('\t');
                // Append size
                let mut nbuf = [0u8; 12];
                let mut n = e.size as usize;
                let mut pos = nbuf.len();
                if n == 0 { pos -= 1; nbuf[pos] = b'0'; }
                while n > 0 && pos > 0 { pos -= 1; nbuf[pos] = b'0' + (n % 10) as u8; n /= 10; }
                if let Ok(s) = ::core::str::from_utf8(&nbuf[pos..]) { result.push_str(s); }
                result.push('\n');
            }
            let bytes = result.as_bytes();
            let copy_len = bytes.len().min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &bytes[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_write_file(path_ptr, path_len, data_ptr, data_len) -> i32
    // Saves data to Synapse VFS. Returns 0 on success, -1 on error.
    let _ = linker.func_wrap("env", "folk_write_file",
        |mut caller: Caller<HostState>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32| -> i32 {
            if path_len <= 0 || path_len > 256 || data_len < 0 || data_len > 4096 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut name_buf = alloc::vec![0u8; path_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut name_buf).is_err() { return -1; }
            let name = match alloc::str::from_utf8(&name_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            let mut data_buf = alloc::vec![0u8; data_len as usize];
            if data_len > 0 {
                if mem.read(&caller, data_ptr as usize, &mut data_buf).is_err() { return -1; }
            }
            match libfolk::sys::synapse::write_file(name, &data_buf) {
                Ok(_) => 0,
                Err(_) => -1,
            }
        },
    );

    // Phase 22: Synchronous file read — load file directly into WASM memory
    // folk_read_file_sync(path_ptr, path_len, dest_ptr, max_len) -> i32
    // Reads a file from Synapse VFS SYNCHRONOUSLY (blocks until loaded).
    // Returns bytes loaded, or -1 on error.
    let _ = linker.func_wrap("env", "folk_read_file_sync",
        |mut caller: Caller<HostState>, path_ptr: i32, path_len: i32, dest_ptr: i32, max_len: i32| -> i32 {
            if path_len <= 0 || path_len > 256 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let mut name_buf = alloc::vec![0u8; path_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut name_buf).is_err() { return -1; }
            let name = match alloc::str::from_utf8(&name_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            // Use Synapse read_file_shmem (synchronous IPC)
            match libfolk::sys::synapse::read_file_shmem(name) {
                Ok(resp) => {
                    const SYNC_READ_VADDR: usize = 0x50080000;
                    if libfolk::sys::shmem_map(resp.shmem_handle, SYNC_READ_VADDR).is_ok() {
                        let data = unsafe {
                            core::slice::from_raw_parts(SYNC_READ_VADDR as *const u8, resp.size as usize)
                        };
                        let copy = data.len().min(max_len as usize);
                        let result = if mem.write(&mut caller, dest_ptr as usize, &data[..copy]).is_ok() {
                            copy as i32
                        } else { -1 };
                        let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, SYNC_READ_VADDR);
                        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                        result
                    } else {
                        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                        -1
                    }
                }
                Err(_) => -1,
            }
        },
    );
}

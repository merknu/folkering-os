//! IPC message handling and helper functions for the compositor.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use compositor::Compositor;
use compositor::window_manager::{WindowManager, Window, UiWidget, count_focusable};
use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};
use libfolk::println;
use core::sync::atomic::{AtomicU32, Ordering};

// IPC message types
pub const MSG_CREATE_WINDOW: u64 = 0x01;
pub const MSG_UPDATE: u64 = 0x02;
pub const MSG_CLOSE: u64 = 0x03;
pub const MSG_CREATE_UI_WINDOW: u64 = 0x06;
pub const MSG_QUERY_NAME: u64 = 0x10;
pub const MSG_QUERY_FOCUS: u64 = 0x11;
/// Register a granted display-list ring. Request: opcode | (shmem_id << 8).
/// Reply: slot index on success, u64::MAX on failure.
pub const MSG_GFX_REGISTER_RING: u64 = 0x20;
/// Unregister a previously registered ring. Request: opcode | (slot << 8).
/// Reply: 0 on success, u64::MAX on failure.
pub const MSG_GFX_UNREGISTER_RING: u64 = 0x21;
/// Bind an input ring to an existing gfx slot.
/// Request: opcode | (slot << 8) | (input_shmem_id << 16).
/// Reply: 0 on success, u64::MAX on failure.
pub const MSG_GFX_REGISTER_INPUT_RING: u64 = 0x22;

/// Execute a tool call and write result back to TokenRing for AI feedback.
/// Shows brief status in window; full result goes to ring for KV-cache injection.
pub fn execute_tool_call(
    tool_content: &str,
    win: &mut compositor::window_manager::Window,
    ring_vaddr: usize,
    write_idx: usize,
) {
    use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};
    use core::sync::atomic::{AtomicU32, Ordering};

    let trimmed = tool_content.trim();
    let (cmd, args) = if let Some(pos) = trimmed.find(' ') {
        (&trimmed[..pos], trimmed[pos + 1..].trim())
    } else {
        (trimmed, "")
    };

    const TOOL_SHMEM_VADDR: usize = 0x30000000;
    const PREFIX: &[u8] = b"\n<|tool_result|>";
    const SUFFIX: &[u8] = b"<|/tool_result|>\n";

    // Stack buffer for tool result content (8KB -- large enough for Gemini responses)
    let mut result_buf = [0u8; 8192];
    let mut result_len: usize = 0;

    match cmd {
        "write" => {
            if let Some(pos) = args.find(' ') {
                let filename = args[..pos].trim();
                let content = args[pos + 1..].trim();
                if filename.is_empty() || content.is_empty() {
                    result_len = copy_str(b"Error: write requires FILENAME CONTENT", &mut result_buf);
                } else if filename.contains("..") || content.len() > 4096 {
                    result_len = copy_str(b"Error: write denied (security)", &mut result_buf);
                } else {
                    match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                        Ok(()) => {
                            result_len = copy_str(b"OK: File written: ", &mut result_buf);
                            let fname_bytes = filename.as_bytes();
                            let add = fname_bytes.len().min(result_buf.len() - result_len);
                            result_buf[result_len..result_len + add].copy_from_slice(&fname_bytes[..add]);
                            result_len += add;
                        }
                        Err(_) => result_len = copy_str(b"Error: Write failed", &mut result_buf),
                    }
                }
            } else {
                result_len = copy_str(b"Error: write requires FILENAME CONTENT", &mut result_buf);
            }
        }
        "read" => {
            if args.is_empty() {
                result_len = copy_str(b"Error: read requires FILENAME", &mut result_buf);
            } else {
                match libfolk::sys::synapse::read_file_shmem(args) {
                    Ok(resp) => {
                        if shmem_map(resp.shmem_handle, TOOL_SHMEM_VADDR).is_ok() {
                            let data = unsafe {
                                core::slice::from_raw_parts(
                                    TOOL_SHMEM_VADDR as *const u8,
                                    (resp.size as usize).min(4096),
                                )
                            };
                            // Copy file content (safe UTF-8 truncation by lines)
                            if let Ok(text) = core::str::from_utf8(data) {
                                for line in text.lines().take(8) {
                                    let lb = line.as_bytes();
                                    let add = lb.len().min(result_buf.len() - result_len);
                                    if add == 0 { break; }
                                    result_buf[result_len..result_len + add].copy_from_slice(&lb[..add]);
                                    result_len += add;
                                    if result_len < result_buf.len() {
                                        result_buf[result_len] = b'\n';
                                        result_len += 1;
                                    }
                                }
                            } else {
                                result_len = copy_str(b"(binary data)", &mut result_buf);
                            }
                            let _ = shmem_unmap(resp.shmem_handle, TOOL_SHMEM_VADDR);
                        }
                        let _ = shmem_destroy(resp.shmem_handle);
                    }
                    Err(_) => result_len = copy_str(b"Error: File not found", &mut result_buf),
                }
            }
        }
        "ls" => {
            result_len = copy_str(b"Files: (listing not yet implemented)", &mut result_buf);
        }
        "ask_gemini" => {
            if args.is_empty() {
                result_len = copy_str(b"Error: ask_gemini requires a prompt", &mut result_buf);
            } else {
                // Large response buffer for cloud AI responses (128KB via mmap)
                const GEMINI_BUF_SIZE: usize = 131072;
                const GEMINI_BUF_VADDR: usize = 0x32000000;

                // Allocate anonymous memory for response
                if libfolk::sys::mmap_at(GEMINI_BUF_VADDR, GEMINI_BUF_SIZE, 3).is_ok() {
                    let gemini_buf = unsafe {
                        core::slice::from_raw_parts_mut(GEMINI_BUF_VADDR as *mut u8, GEMINI_BUF_SIZE)
                    };

                    win.push_line("[tool] Asking Gemini...");

                    let response_len = libfolk::sys::ask_gemini(args, gemini_buf);

                    if response_len > 0 {
                        // Truncate to fit in ring buffer (max 8KB for tool result)
                        let usable = response_len.min(8000);
                        result_len = usable.min(result_buf.len());
                        result_buf[..result_len].copy_from_slice(&gemini_buf[..result_len]);
                    } else {
                        result_len = copy_str(b"Error: Cloud API unreachable", &mut result_buf);
                    }

                    // Free the buffer
                    let _ = libfolk::sys::munmap(GEMINI_BUF_VADDR as *mut u8, GEMINI_BUF_SIZE);
                } else {
                    result_len = copy_str(b"Error: memory allocation failed", &mut result_buf);
                }
            }
        }
        _ => {
            result_len = copy_str(b"Error: Unknown tool command", &mut result_buf);
        }
    }

    // Show brief status in window
    win.push_line("[tool] Executed: ");
    win.append_text(cmd.as_bytes());

    // Write result back to ring for inference-server to consume
    let total_len = PREFIX.len() + result_len + SUFFIX.len();
    let available = super::RING_DATA_MAX.saturating_sub(write_idx);

    if total_len <= available && ring_vaddr != 0 {
        unsafe {
            let base = (ring_vaddr as *mut u8).add(super::RING_HEADER_SIZE).add(write_idx);
            core::ptr::copy_nonoverlapping(PREFIX.as_ptr(), base, PREFIX.len());
            core::ptr::copy_nonoverlapping(
                result_buf.as_ptr(),
                base.add(PREFIX.len()),
                result_len,
            );
            core::ptr::copy_nonoverlapping(
                SUFFIX.as_ptr(),
                base.add(PREFIX.len() + result_len),
                SUFFIX.len(),
            );
        }

        // Signal inference-server: result ready
        let tool_result_len = unsafe { &*((ring_vaddr + 12) as *const AtomicU32) };
        let tool_state = unsafe { &*((ring_vaddr + 8) as *const AtomicU32) };
        tool_result_len.store(total_len as u32, Ordering::Release);
        tool_state.store(2, Ordering::Release); // 2 = result_ready
    }
}

/// Helper: copy bytes into buffer, return length copied
pub fn copy_str(src: &[u8], dst: &mut [u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

pub fn clamp_focus(wm: &mut WindowManager, win_id: u32) {
    if let Some(win) = wm.get_window_mut(win_id) {
        if let Some(idx) = win.focused_widget {
            let fc = compositor::window_manager::count_focusable(&win.widgets);
            if fc == 0 {
                win.focused_widget = None;
            } else if idx >= fc {
                win.focused_widget = Some(fc - 1);
            }
        }
    }
}

/// Update a window's widget tree from a shmem UI buffer.
/// Maps the shmem, parses the FKUI header and widget tree,
/// replaces the window's widgets in-place, then cleans up shmem.
pub fn update_window_widgets(wm: &mut WindowManager, win_id: u32, shmem_handle: u32) {
    if shmem_map(shmem_handle, super::COMPOSITOR_SHMEM_VADDR).is_ok() {
        let buf = unsafe {
            core::slice::from_raw_parts(super::COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
        };
        if let Some(header) = libfolk::ui::parse_header(buf) {
            let (root, _) = parse_widget_tree(header.widget_data);
            if let Some(widget) = root {
                if let Some(win) = wm.get_window_mut(win_id) {
                    // clear() + push reuses Vec capacity -- no new allocation in bump allocator
                    win.widgets.clear();
                    win.widgets.push(widget);
                }
            }
        }
        let _ = shmem_unmap(shmem_handle, super::COMPOSITOR_SHMEM_VADDR);
    }
    let _ = shmem_destroy(shmem_handle);
}

/// Recursively parse widget tree from wire format into UiWidget
pub fn parse_widget_tree(data: &[u8]) -> (Option<compositor::window_manager::UiWidget>, usize) {
    use compositor::window_manager::UiWidget;
    use libfolk::ui::{parse_widget, ParsedWidget as PW};

    match parse_widget(data) {
        Some((PW::Label { text, color }, consumed)) => {
            (Some(UiWidget::label(text, color)), consumed)
        }
        Some((PW::Button { label, action_id, bg, fg }, consumed)) => {
            (Some(UiWidget::button(label, action_id, bg, fg)), consumed)
        }
        Some((PW::Spacer { height }, consumed)) => {
            (Some(UiWidget::Spacer { height }), consumed)
        }
        Some((PW::TextInput { placeholder, action_id, max_len }, consumed)) => {
            (Some(UiWidget::text_input(placeholder, action_id, max_len)), consumed)
        }
        Some((PW::VStackBegin { spacing, child_count }, mut consumed)) => {
            let mut children = alloc::vec::Vec::new();
            for _ in 0..child_count {
                let (child, child_consumed) = parse_widget_tree(&data[consumed..]);
                if let Some(c) = child {
                    children.push(c);
                }
                consumed += child_consumed;
            }
            (Some(UiWidget::VStack { children, spacing }), consumed)
        }
        Some((PW::HStackBegin { spacing, child_count }, mut consumed)) => {
            let mut children = alloc::vec::Vec::new();
            for _ in 0..child_count {
                let (child, child_consumed) = parse_widget_tree(&data[consumed..]);
                if let Some(c) = child {
                    children.push(c);
                }
                consumed += child_consumed;
            }
            (Some(UiWidget::HStack { children, spacing }), consumed)
        }
        _ => (None, 0),
    }
}

/// Handle an incoming IPC message.
///
/// # Protocol (Phase 6.1 - Single Payload)
///
/// All data is packed into payload0 since recv_async() only provides payload0:
///
/// - MSG_CREATE_WINDOW (0x01): opcode only, returns window_id
/// - MSG_UPDATE (0x02): [opcode:8][window:4][node:16][role:8][hash:24]
/// - MSG_CLOSE (0x03): [opcode:8][window:4]
/// - MSG_QUERY_NAME (0x10): [opcode:8][hash:24]
/// - MSG_QUERY_FOCUS (0x11): opcode only
///
/// Returns the response payload.
pub fn handle_message(compositor: &mut Compositor, payload0: u64) -> u64 {
    // Extract opcode from low 8 bits
    let opcode = payload0 & 0xFF;

    match opcode {
        MSG_CREATE_WINDOW => {
            let window_id = compositor.create_window();
            println!("[COMPOSITOR] Created window {}", window_id);
            window_id
        }

        MSG_UPDATE => {
            // Decode: [opcode:8][window:4][node:16][role:8][hash:24]
            let window_id = (payload0 >> 8) & 0xF;
            let node_id = (payload0 >> 12) & 0xFFFF;
            let role = ((payload0 >> 28) & 0xFF) as u8;
            let name_hash = ((payload0 >> 36) & 0xFF_FFFF) as u32;

            // Convert role byte to Role enum
            let role_enum = role_from_u8(role);

            // Create node with name that will hash to the same value
            let node = libaccesskit_folk::Node::new(role_enum)
                .with_name(format_hash_name(name_hash));

            // Create TreeUpdate with single node
            let update = libaccesskit_folk::TreeUpdate::new(node_id)
                .with_node(node_id, node);

            // Process update
            if compositor.handle_update(window_id, update).is_ok() {
                println!("[COMPOSITOR] Updated win {} node {} (role={}, hash={:#x})",
                         window_id, node_id, role, name_hash);
                0
            } else {
                println!("[COMPOSITOR] Update failed for window {}", window_id);
                u64::MAX
            }
        }

        MSG_CLOSE => {
            let window_id = (payload0 >> 8) & 0xF;
            compositor.handle_close(window_id);
            println!("[COMPOSITOR] Closed window {}", window_id);
            0
        }

        MSG_QUERY_NAME => {
            // Decode: [opcode:8][hash:24]
            let name_hash = ((payload0 >> 8) & 0xFF_FFFF) as u32;

            match compositor.world.find_by_name_hash(name_hash) {
                Some((window_id, node_id, _node)) => {
                    println!("[COMPOSITOR] Query: found node {} in window {} (hash={:#x})",
                             node_id, window_id, name_hash);
                    // Pack: window_id in upper 32 bits, node_id in lower 32 bits
                    ((window_id as u64) << 32) | (node_id & 0xFFFF_FFFF)
                }
                None => {
                    println!("[COMPOSITOR] Query: not found (hash={:#x})", name_hash);
                    u64::MAX
                }
            }
        }

        MSG_QUERY_FOCUS => {
            match compositor.world.get_focus() {
                Some((window_id, node_id, _node)) => {
                    ((window_id as u64) << 32) | (node_id & 0xFFFF_FFFF)
                }
                None => u64::MAX
            }
        }

        MSG_GFX_REGISTER_RING => {
            // Payload: opcode (8) | shmem_id (32). Higher bits unused.
            let shmem_id = ((payload0 >> 8) & 0xFFFF_FFFF) as u32;
            match compositor::gfx_rings::register(shmem_id) {
                Ok(slot) => {
                    println!("[COMPOSITOR] Registered gfx ring shmem={} -> slot {}", shmem_id, slot);
                    slot as u64
                }
                Err(e) => {
                    println!("[COMPOSITOR] gfx ring register failed: {:?}", e);
                    u64::MAX
                }
            }
        }

        MSG_GFX_UNREGISTER_RING => {
            let slot = ((payload0 >> 8) & 0xFF) as u32;
            match compositor::gfx_rings::unregister(slot) {
                Ok(()) => {
                    println!("[COMPOSITOR] Unregistered gfx ring slot {}", slot);
                    0
                }
                Err(_) => u64::MAX,
            }
        }

        MSG_GFX_REGISTER_INPUT_RING => {
            // payload0: op (8) | slot (8) | shmem_id (32). slot bits
            // 8..16, shmem_id bits 16..48.
            let slot = ((payload0 >> 8) & 0xFF) as u32;
            let shmem_id = ((payload0 >> 16) & 0xFFFF_FFFF) as u32;
            match compositor::gfx_rings::register_input(slot, shmem_id) {
                Ok(()) => {
                    println!("[COMPOSITOR] Bound input ring shmem={} -> gfx slot {}",
                        shmem_id, slot);
                    0
                }
                Err(e) => {
                    println!("[COMPOSITOR] register_input failed: {:?}", e);
                    u64::MAX
                }
            }
        }

        _ => {
            println!("[COMPOSITOR] Unknown opcode: {:#x}", opcode);
            u64::MAX
        }
    }
}

/// Convert role byte to Role enum
pub fn role_from_u8(role: u8) -> libaccesskit_folk::Role {
    match role {
        0 => libaccesskit_folk::Role::Unknown,
        1 => libaccesskit_folk::Role::Window,
        2 => libaccesskit_folk::Role::Group,
        3 => libaccesskit_folk::Role::ScrollView,
        4 => libaccesskit_folk::Role::TabPanel,
        5 => libaccesskit_folk::Role::Dialog,
        6 => libaccesskit_folk::Role::Alert,
        10 => libaccesskit_folk::Role::Button,
        11 => libaccesskit_folk::Role::Checkbox,
        12 => libaccesskit_folk::Role::RadioButton,
        13 => libaccesskit_folk::Role::ComboBox,
        14 => libaccesskit_folk::Role::MenuItem,
        15 => libaccesskit_folk::Role::Link,
        16 => libaccesskit_folk::Role::Slider,
        17 => libaccesskit_folk::Role::Tab,
        20 => libaccesskit_folk::Role::StaticText,
        21 => libaccesskit_folk::Role::TextInput,
        22 => libaccesskit_folk::Role::TextArea,
        23 => libaccesskit_folk::Role::Label,
        24 => libaccesskit_folk::Role::Heading,
        30 => libaccesskit_folk::Role::Image,
        31 => libaccesskit_folk::Role::ProgressBar,
        32 => libaccesskit_folk::Role::Separator,
        40 => libaccesskit_folk::Role::List,
        41 => libaccesskit_folk::Role::ListItem,
        42 => libaccesskit_folk::Role::Table,
        43 => libaccesskit_folk::Role::TableRow,
        44 => libaccesskit_folk::Role::TableCell,
        45 => libaccesskit_folk::Role::Tree,
        46 => libaccesskit_folk::Role::TreeItem,
        _ => libaccesskit_folk::Role::Unknown,
    }
}

/// Format a hash as a name string.
/// Phase 6.1 workaround: we store the hash directly as a hex string
/// so that find_by_name_hash can match it.
/// Note: We use 6 hex digits (24 bits) to match the IPC encoding.
pub fn format_hash_name(hash: u32) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;

    let mut s = String::new();
    // Use 6 hex digits to match the 24-bit truncated hash from IPC
    let _ = write!(s, "__hash_{:06x}", hash & 0xFF_FFFF);
    s
}

/// Format IQE telemetry line for COM3 export: "IQE,KBD,1234\n"
pub fn fmt_iqe_line(buf: &mut [u8], tag: &[u8], val: u64) -> usize {
    let mut i = 0;
    buf[i]=b'I'; i+=1; buf[i]=b'Q'; i+=1; buf[i]=b'E'; i+=1; buf[i]=b','; i+=1;
    for &b in tag { if i < buf.len() { buf[i] = b; i += 1; } }
    if i < buf.len() { buf[i] = b','; i += 1; }
    i += fmt_u64_into(&mut buf[i..], val);
    if i < buf.len() { buf[i] = b'\n'; i += 1; }
    i
}

/// Format u64 as decimal ASCII into buffer, return bytes written.
pub fn fmt_u64_into(buf: &mut [u8], mut val: u64) -> usize {
    if buf.is_empty() { return 0; }
    if val == 0 { buf[0] = b'0'; return 1; }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while val > 0 && i < 20 { tmp[i] = b'0' + (val % 10) as u8; val /= 10; i += 1; }
    let len = i.min(buf.len());
    for j in 0..len { buf[j] = tmp[i - 1 - j]; }
    len
}

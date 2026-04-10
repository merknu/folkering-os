//! System commands: time, random, poweroff, test_gui.

use libfolk::{print, println};
use libfolk::sys::compositor::{
    create_window, find_node_by_hash, hash_name as comp_hash_name, role, update_node,
};
use libfolk::sys::poweroff;

use crate::state::save_all_app_states;

pub fn cmd_time() {
    let ts = libfolk::sys::time::unix_timestamp();
    let secs_per_day: u64 = 86400;
    let secs_per_hour: u64 = 3600;
    let secs_per_min: u64 = 60;
    let days_in_month: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let mut remaining = ts;
    let mut year: u64 = 1970;
    loop {
        let days_in_year = if (year % 400 == 0) || (year % 4 == 0 && year % 100 != 0) { 366 } else { 365 };
        if remaining < days_in_year * secs_per_day { break; }
        remaining -= days_in_year * secs_per_day;
        year += 1;
    }
    let is_leap = (year % 400 == 0) || (year % 4 == 0 && year % 100 != 0);
    let mut month: u64 = 1;
    for m in 0..12 {
        let mut d = days_in_month[m];
        if m == 1 && is_leap { d += 1; }
        if remaining < d * secs_per_day { break; }
        remaining -= d * secs_per_day;
        month += 1;
    }
    let day = remaining / secs_per_day + 1;
    remaining %= secs_per_day;
    let hour = remaining / secs_per_hour;
    remaining %= secs_per_hour;
    let min = remaining / secs_per_min;
    let sec = remaining % secs_per_min;

    println!("{}-{:02}-{:02} {:02}:{:02}:{:02} UTC", year, month, day, hour, min, sec);
    println!("Unix timestamp: {}", ts);
}

pub fn cmd_random() {
    println!("Random values (RDRAND/RDTSC):");
    for i in 0..4 {
        let val = libfolk::sys::random::random_u64();
        print!("  [{}] 0x", i);
        for shift in (0..16).rev() {
            let nibble = ((val >> (shift * 4)) & 0xF) as u8;
            let c = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
            print!("{}", c as char);
        }
        println!();
    }
}

pub fn cmd_poweroff() {
    save_all_app_states();
    println!("Shutting down...");
    poweroff();
}

/// Test Semantic Mirror integration: create a window, send a button,
/// query for it, verify roundtrip.
pub fn cmd_test_gui() {
    println!("=== Semantic Mirror Integration Test ===\n");

    println!("[1] Creating window...");
    let window_id = match create_window() {
        Ok(id) => { println!("    Window created: {}", id); id }
        Err(e) => {
            println!("    FAIL: {:?}", e);
            println!("\n    Hint: Is the compositor running?");
            return;
        }
    };

    println!("[2] Sending 'Submit Form' button...");
    let button_name = "Submit Form";
    let name_hash = comp_hash_name(button_name);
    let node_id: u64 = 42;

    match update_node(window_id, node_id, role::BUTTON, name_hash) {
        Ok(()) => println!("    TreeUpdate sent OK"),
        Err(_) => { println!("    TreeUpdate FAIL"); return; }
    }

    println!("[3] Querying...");
    match find_node_by_hash(name_hash) {
        Ok((true, found_node_id, found_window_id)) => {
            if found_node_id == node_id && found_window_id == window_id {
                println!("[SUCCESS] Semantic Mirror verified!");
            } else {
                println!("[FAIL] Node/window mismatch");
            }
        }
        Ok((false, _, _)) => println!("[FAIL] Node not found"),
        Err(_) => println!("[FAIL] Query error"),
    }
}

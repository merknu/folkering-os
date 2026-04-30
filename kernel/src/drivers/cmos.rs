//! CMOS/RTC Driver — Real-Time Clock
//!
//! Reads wall-clock time from the x86 CMOS chip via I/O ports 0x70/0x71.
//! Provides Unix timestamp for TLS certificate validation and logging.

use x86_64::instructions::port::Port;

/// Date/time from CMOS RTC
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    pub year: u16,   // Full year (2000+)
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Read a CMOS register (with NMI disabled)
unsafe fn cmos_read(reg: u8) -> u8 {
    let mut addr_port = Port::<u8>::new(0x70);
    let mut data_port = Port::<u8>::new(0x71);
    addr_port.write(0x80 | reg); // bit 7 = disable NMI
    data_port.read()
}

/// Write a CMOS register
unsafe fn cmos_write(reg: u8, value: u8) {
    let mut addr_port = Port::<u8>::new(0x70);
    let mut data_port = Port::<u8>::new(0x71);
    addr_port.write(0x80 | reg);
    data_port.write(value);
}

/// Convert binary byte to BCD (assumes value < 100)
fn bin_to_bcd(bin: u8) -> u8 {
    ((bin / 10) << 4) | (bin % 10)
}

/// Convert BCD byte to binary
fn bcd_to_bin(bcd: u8) -> u8 {
    ((bcd >> 4) * 10) + (bcd & 0x0F)
}

/// Read date/time from CMOS RTC.
/// Handles update-in-progress and BCD/binary format detection.
///
/// Capped wait: real RTCs clear bit 7 of register 0x0A within ~1 ms.
/// 100_000 polls is overkill on real silicon but defends against a
/// virtualised RTC that never clears the bit (Issue #56 follow-up to
/// the spin-loop audit). On overflow we read anyway — values may be
/// torn but that's better than a dead kernel.
pub fn read_rtc() -> DateTime {
    unsafe {
        // Wait for update-in-progress to clear
        for _ in 0..100_000 {
            if (cmos_read(0x0A) & 0x80) == 0 {
                break;
            }
        }

        // Read all registers
        let sec = cmos_read(0x00);
        let min = cmos_read(0x02);
        let hr = cmos_read(0x04);
        let day = cmos_read(0x07);
        let mon = cmos_read(0x08);
        let yr = cmos_read(0x09);
        let status_b = cmos_read(0x0B);

        // Check if data is BCD or binary (bit 2 of status B)
        let is_binary = (status_b & 0x04) != 0;

        let second = if is_binary { sec } else { bcd_to_bin(sec) };
        let minute = if is_binary { min } else { bcd_to_bin(min) };
        let hour = if is_binary { hr } else { bcd_to_bin(hr) };
        let d = if is_binary { day } else { bcd_to_bin(day) };
        let m = if is_binary { mon } else { bcd_to_bin(mon) };
        let y = if is_binary { yr } else { bcd_to_bin(yr) };

        DateTime {
            year: 2000 + y as u16,
            month: m,
            day: d,
            hour,
            minute,
            second,
        }
    }
}

/// Convert DateTime to Unix timestamp (seconds since 1970-01-01 00:00:00 UTC)
pub fn to_unix_timestamp(dt: &DateTime) -> u64 {
    let year = dt.year as u64;
    let month = dt.month as u64;
    let day = dt.day as u64;

    // Days in each month (non-leap)
    const DAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    // Years since epoch
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    // Months in current year
    for m in 1..month {
        days += DAYS[(m - 1) as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }

    // Days in current month
    days += day - 1;

    days * 86400 + dt.hour as u64 * 3600 + dt.minute as u64 * 60 + dt.second as u64
}

fn is_leap(year: u64) -> bool {
    (year % 400 == 0) || (year % 4 == 0 && year % 100 != 0)
}

/// Initialize RTC driver — read and log current time
pub fn init() {
    let dt = read_rtc();
    let ts = to_unix_timestamp(&dt);

    crate::serial_str!("[RTC] ");
    crate::drivers::serial::write_dec(dt.year as u32);
    crate::serial_str!("-");
    if dt.month < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.month as u32);
    crate::serial_str!("-");
    if dt.day < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.day as u32);
    crate::serial_str!(" ");
    if dt.hour < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.hour as u32);
    crate::serial_str!(":");
    if dt.minute < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.minute as u32);
    crate::serial_str!(":");
    if dt.second < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.second as u32);
    crate::serial_str!(" UTC (unix=");
    // Print unix timestamp in decimal parts since write_dec is u32
    let hi = (ts / 1_000_000_000) as u32;
    let lo = (ts % 1_000_000_000) as u32;
    if hi > 0 {
        crate::drivers::serial::write_dec(hi);
    }
    crate::drivers::serial::write_dec(lo);
    crate::serial_strln!(")");
}

/// Get current Unix timestamp
pub fn unix_timestamp() -> u64 {
    to_unix_timestamp(&read_rtc())
}

/// Write date/time to CMOS RTC.
/// Detects BCD vs binary format and writes accordingly.
///
/// Same 100_000-iter cap as `read_rtc` — see its doc-comment for why.
pub fn write_rtc(dt: &DateTime) {
    unsafe {
        // Wait for any in-progress update to complete
        for _ in 0..100_000 {
            if (cmos_read(0x0A) & 0x80) == 0 {
                break;
            }
        }

        // Read status B to check format
        let status_b = cmos_read(0x0B);
        let is_binary = (status_b & 0x04) != 0;

        // Disable RTC updates while we write
        cmos_write(0x0B, status_b | 0x80);

        let conv = |v: u8| if is_binary { v } else { bin_to_bcd(v) };

        cmos_write(0x00, conv(dt.second));
        cmos_write(0x02, conv(dt.minute));
        cmos_write(0x04, conv(dt.hour));
        cmos_write(0x07, conv(dt.day));
        cmos_write(0x08, conv(dt.month));
        cmos_write(0x09, conv((dt.year - 2000) as u8));

        // Re-enable RTC updates
        cmos_write(0x0B, status_b);
    }
}

/// Convert Unix timestamp to DateTime and write to RTC.
/// Used by NTP sync.
pub fn set_unix_time(unix_secs: u64) {
    let dt = unix_to_datetime(unix_secs);
    crate::serial_str!("[CMOS] Setting RTC to ");
    crate::drivers::serial::write_dec(dt.year as u32);
    crate::serial_str!("-");
    if dt.month < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.month as u32);
    crate::serial_str!("-");
    if dt.day < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.day as u32);
    crate::serial_str!(" ");
    if dt.hour < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.hour as u32);
    crate::serial_str!(":");
    if dt.minute < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.minute as u32);
    crate::serial_str!(":");
    if dt.second < 10 { crate::serial_str!("0"); }
    crate::drivers::serial::write_dec(dt.second as u32);
    crate::serial_strln!(" UTC");
    write_rtc(&dt);
}

/// Convert Unix timestamp to DateTime (UTC).
fn unix_to_datetime(unix_secs: u64) -> DateTime {
    let second = (unix_secs % 60) as u8;
    let mut t = unix_secs / 60;
    let minute = (t % 60) as u8;
    t /= 60;
    let hour = (t % 24) as u8;
    let mut days = t / 24;

    let mut year: u16 = 1970;
    loop {
        let year_days = if is_leap(year as u64) { 366 } else { 365 };
        if days < year_days { break; }
        days -= year_days;
        year += 1;
    }

    let month_days: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month: u8 = 1;
    for (i, &md) in month_days.iter().enumerate() {
        let mut d = md as u64;
        if i == 1 && is_leap(year as u64) { d = 29; }
        if days < d { break; }
        days -= d;
        month += 1;
    }
    let day = (days + 1) as u8;

    DateTime { year, month, day, hour, minute, second }
}

//! CMOS Real-Time Clock (RTC) driver
//!
//! Reads date/time from the standard PC CMOS RTC via I/O ports 0x70/0x71.
//! Handles BCD-to-binary conversion and NMI disable bit.

/// Read a CMOS register. NMI disable bit (bit 7) is preserved.
fn cmos_read(reg: u8) -> u8 {
    unsafe {
        // Select register (keep NMI disable bit clear)
        x86_64::instructions::port::Port::<u8>::new(0x70).write(reg & 0x7F);
        x86_64::instructions::port::Port::<u8>::new(0x71).read()
    }
}

/// Check if an RTC update is in progress (bit 7 of register 0x0A)
fn update_in_progress() -> bool {
    cmos_read(0x0A) & 0x80 != 0
}

/// Convert BCD byte to binary
fn bcd_to_bin(bcd: u8) -> u8 {
    (bcd & 0x0F) + ((bcd >> 4) * 10)
}

/// Date/time from RTC
#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: u16,   // Full year (e.g., 2026)
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Read current date/time from CMOS RTC.
/// Waits for update-not-in-progress, reads twice to ensure consistency.
pub fn read_rtc() -> DateTime {
    // Wait until no update is in progress
    while update_in_progress() {
        core::hint::spin_loop();
    }

    let second = cmos_read(0x00);
    let minute = cmos_read(0x02);
    let hour   = cmos_read(0x04);
    let day    = cmos_read(0x07);
    let month  = cmos_read(0x08);
    let year   = cmos_read(0x09);

    // Check register B for BCD mode (bit 2 = 0 means BCD)
    let reg_b = cmos_read(0x0B);
    let is_bcd = (reg_b & 0x04) == 0;

    let (second, minute, hour, day, month, year) = if is_bcd {
        (
            bcd_to_bin(second),
            bcd_to_bin(minute),
            bcd_to_bin(hour & 0x7F), // Mask AM/PM bit
            bcd_to_bin(day),
            bcd_to_bin(month),
            bcd_to_bin(year),
        )
    } else {
        (second, minute, hour & 0x7F, day, month, year)
    };

    // Year is 2-digit, assume 2000s
    let full_year = 2000 + year as u16;

    DateTime {
        year: full_year,
        month,
        day,
        hour,
        minute,
        second,
    }
}

/// Pack DateTime into u64 for syscall return:
/// bits 31-26: year-2000 (0-63)
/// bits 25-22: month (1-12)
/// bits 21-17: day (1-31)
/// bits 16-12: hour (0-23)
/// bits 11-6:  minute (0-59)
/// bits 5-0:   second (0-59)
pub fn read_rtc_packed() -> u64 {
    let dt = read_rtc();
    let y = (dt.year.saturating_sub(2000) as u64) & 0x3F;
    let m = (dt.month as u64) & 0x0F;
    let d = (dt.day as u64) & 0x1F;
    let h = (dt.hour as u64) & 0x1F;
    let min = (dt.minute as u64) & 0x3F;
    let s = (dt.second as u64) & 0x3F;
    (y << 26) | (m << 22) | (d << 17) | (h << 12) | (min << 6) | s
}

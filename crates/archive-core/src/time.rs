//! Shared time utilities used by multiple format parsers.

use std::time::{Duration, SystemTime};

/// Convert a DOS-format date+time pair to `SystemTime`.
///
/// DOS date: bits 15-9 = year offset from 1980, 8-5 = month (1-12), 4-0 = day (1-31)
/// DOS time: bits 15-11 = hour, 10-5 = minute, 4-0 = 2-second count
///
/// Returns `None` if the date is zero (absent) or invalid.
pub(crate) fn dos_datetime_to_system_time(date: u16, time: u16) -> Option<SystemTime> {
    if date == 0 {
        return None;
    }

    let year = ((date >> 9) & 0x7F) as u32 + 1980;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as u64;
    let minute = ((time >> 5) & 0x3F) as u64;
    let second = ((time & 0x1F) as u64) * 2;

    if month == 0 || month > 12 || day == 0 || day > 31 {
        return None;
    }

    // Compute days since Unix epoch (1970-01-01).
    // We use a simple Gregorian calendar calculation.
    let days_since_epoch = days_since_unix_epoch(year, month, day)?;

    let secs = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second;

    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Compute the number of days between 1970-01-01 and the given Gregorian date.
/// Returns `None` for dates before 1970 (shouldn't happen in ZIP's 1980+ range).
pub(crate) fn days_since_unix_epoch(year: u32, month: u32, day: u32) -> Option<u64> {
    // Days in each month (non-leap year)
    const DAYS_IN_MONTH: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    if year < 1970 {
        return None;
    }

    let is_leap = |y: u32| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));

    let mut days: u64 = 0;

    // Full years from 1970 to year-1
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    // Full months from January to month-1 in the target year
    for m in 1..month {
        let m_idx = (m - 1) as usize;
        days += DAYS_IN_MONTH[m_idx] as u64;
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }

    // Days in the target month (day is 1-based)
    days += (day - 1) as u64;

    Some(days)
}

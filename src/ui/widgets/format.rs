//! Formatting helpers shared by the widgets that show file attributes
//! (the pane list, the comparison view, the preview's binary summary).
//!
//! # Public API (Wave 3 seam pass)
//!
//! ```text
//! /// Most cells `format_size` ever takes.
//! pub const SIZE_WIDTH: u16 = 8;
//! /// `999 B`, `12.4 MiB`, `512 GiB`: binary units, one decimal below 100.
//! pub fn format_size(bytes: u64) -> String;
//! /// Local `YYYY-MM-DD HH:MM`; `None` when the time cannot be represented.
//! pub fn format_local_mtime(mtime: i64) -> Option<String>;
//! ```

use std::mem::MaybeUninit;

/// Most cells [`format_size`] ever takes (`16.0 EiB`, `999 KiB`).
pub const SIZE_WIDTH: u16 = 8;

/// Human-readable size with binary units, at most [`SIZE_WIDTH`] cells: `999 B`,
/// `12.4 MiB`, `512 GiB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    // Precision loss above 2^53 bytes is far below the one shown decimal.
    let mut value = bytes as f64;
    let mut unit = 0;
    while unit < UNITS.len() - 1 && value >= 999.5 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 99.95 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// `mtime` (seconds since the epoch) in the local time zone, or `None` when
/// the time cannot be represented.
pub fn format_local_mtime(mtime: i64) -> Option<String> {
    format_mtime(mtime, local_utc_offset(mtime)?)
}

/// The local time zone's offset from UTC, in seconds, at `time` (so daylight
/// saving time applies per timestamp).
fn local_utc_offset(time: i64) -> Option<i64> {
    let time = libc::time_t::try_from(time).ok()?;
    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` is the reentrant variant; it only reads `time`
    // and writes the `tm` it is given, and returns null (leaving `tm`
    // unspecified) on failure, which is checked before `tm` is read.
    let result = unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    // SAFETY: `localtime_r` succeeded, so it fully initialized `tm`.
    let tm = unsafe { tm.assume_init() };
    Some(tm.tm_gmtoff)
}

/// `YYYY-MM-DD HH:MM` for `time` seconds since the epoch shifted by
/// `offset` seconds; `None` on overflow.
fn format_mtime(time: i64, offset: i64) -> Option<String> {
    let local = time.checked_add(offset)?;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60
    ))
}

/// Proleptic Gregorian date of `days` since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`); `None` on overflow.
fn civil_from_days(days: i64) -> Option<(i64, i64, i64)> {
    let z = days.checked_add(719_468)?;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era.checked_mul(400)?.checked_add(yoe)? + i64::from(month <= 2);
    Some((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units_within_eight_cells() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(13_002_342), "12.4 MiB");
        assert_eq!(format_size(512 * 1024 * 1024 * 1024), "512 GiB");
        assert_eq!(format_size(1_023_487), "999 KiB");
        assert_eq!(format_size(1_023_488), "1.0 MiB");
        assert_eq!(format_size(u64::MAX), "16.0 EiB");
        for bytes in [999, 99_999, 1_048_575, u64::MAX / 3] {
            assert!(format_size(bytes).len() <= usize::from(SIZE_WIDTH));
        }
    }

    #[test]
    fn mtime_formats_civil_time_with_offset() {
        assert_eq!(format_mtime(0, 0).as_deref(), Some("1970-01-01 00:00"));
        assert_eq!(
            format_mtime(1_700_000_000, 0).as_deref(),
            Some("2023-11-14 22:13")
        );
        assert_eq!(
            format_mtime(1_700_000_000, 8 * 3600).as_deref(),
            Some("2023-11-15 06:13")
        );
        assert_eq!(
            format_mtime(951_782_400, 0).as_deref(),
            Some("2000-02-29 00:00")
        );
        assert_eq!(format_mtime(-1, 0).as_deref(), Some("1969-12-31 23:59"));
        assert_eq!(format_mtime(i64::MAX, 1), None);
    }

    #[test]
    fn local_offset_is_within_a_day() {
        let offset = local_utc_offset(1_700_000_000).expect("representable");
        assert!(offset.abs() < 86_400);
    }
}

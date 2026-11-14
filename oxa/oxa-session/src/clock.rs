//! The local wall clock (date, time, time zone) for the per-turn environment note of
//! `oxa-core`. Session files keep UTC milliseconds; this is only what the model is told.

/// A local date and time, as the C library's `localtime_r` gives it (honours `TZ`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTime {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `Monday` .. `Sunday`.
    pub weekday: &'static str,
    /// `HH:MM`, 24-hour.
    pub hm: String,
    /// The zone abbreviation (`CET`, `UTC`); empty when the C library has none.
    pub zone: String,
    /// Offset from UTC in minutes (east positive).
    pub offset_min: i32,
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

impl LocalTime {
    /// `UTC+01:00`, `UTC-03:30`, `UTC`.
    pub fn utc_offset(&self) -> String {
        if self.offset_min == 0 {
            return "UTC".to_string();
        }
        let sign = if self.offset_min < 0 { '-' } else { '+' };
        let m = self.offset_min.unsigned_abs();
        format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
    }

    /// The zone abbreviation, else the offset (`13:42 CET`, `13:42 UTC+05:30`).
    pub fn zone_label(&self) -> String {
        let z = self.zone.trim();
        if z.is_empty() || z.starts_with(['+', '-']) {
            self.utc_offset()
        } else {
            z.to_string()
        }
    }
}

/// The local time at `unix_secs`; `None` when the C library cannot convert it.
pub fn local_time(unix_secs: i64) -> Option<LocalTime> {
    // `time_t` is `i64` on the 64-bit targets oxa ships for; the conversion keeps 32-bit honest.
    #[allow(clippy::useless_conversion)]
    let t: libc::time_t = unix_secs.try_into().ok()?;
    // SAFETY: `tm` is plain old data; `localtime_r` writes it and returns null on failure.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return None;
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null `tm_zone` points at a NUL-terminated string owned by the C library
        // that stays valid until the next tzset; it is copied out right away.
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    Some(LocalTime {
        date: format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
        weekday: WEEKDAYS.get(usize::try_from(tm.tm_wday).ok()?).copied()?,
        hm: format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        zone,
        offset_min: i32::try_from(tm.tm_gmtoff / 60).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_labels() {
        let mut t = LocalTime {
            date: "2026-11-14".into(),
            weekday: "Saturday",
            hm: "13:42".into(),
            zone: "CET".into(),
            offset_min: 60,
        };
        assert_eq!(t.utc_offset(), "UTC+01:00");
        assert_eq!(t.zone_label(), "CET");
        t.zone = "+0530".into();
        t.offset_min = 330;
        assert_eq!(t.zone_label(), "UTC+05:30");
        t.offset_min = -210;
        assert_eq!(t.utc_offset(), "UTC-03:30");
        t.offset_min = 0;
        assert_eq!(t.utc_offset(), "UTC");
    }

    #[test]
    fn converts_a_real_instant() {
        // Whatever the host's zone, the result is well formed.
        let t = local_time(1_794_660_000).expect("localtime");
        assert_eq!(t.date.len(), 10);
        assert_eq!(t.hm.len(), 5);
        assert!(WEEKDAYS.contains(&t.weekday));
    }
}

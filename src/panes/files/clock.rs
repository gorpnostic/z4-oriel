//! Local wall-clock time without a date crate: asks the OS directly (Win32 on Windows, localtime_r elsewhere),
//! so it is instant and gets DST right for old dates too. Shared by files (modified times) and notes.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Local {
    pub year: i32,
    pub month: u32, // 1-12
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn secs_of(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(windows)]
pub fn local(secs: i64) -> Local {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SysTime {
        year: u16,
        month: u16,
        dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }
    #[repr(C)]
    struct FileTime {
        lo: u32,
        hi: u32,
    }
    unsafe extern "system" {
        fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SysTime) -> i32;
        fn SystemTimeToTzSpecificLocalTime(tz: *const std::ffi::c_void, utc: *const SysTime, local: *mut SysTime) -> i32;
    }
    let ticks = ((secs + 11_644_473_600).max(0) as u64) * 10_000_000;
    let ft = FileTime { lo: ticks as u32, hi: (ticks >> 32) as u32 };
    let (mut utc, mut loc) = (SysTime::default(), SysTime::default());
    // SAFETY: plain Win32 calls on stack structs laid out as the API expects
    let ok = unsafe { FileTimeToSystemTime(&ft, &mut utc) != 0 && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) != 0 };
    let s = if ok { loc } else { utc };
    Local { year: s.year as i32, month: s.month as u32, day: s.day as u32, hour: s.hour as u32, min: s.minute as u32, sec: s.second as u32 }
}

#[cfg(unix)]
pub fn local(secs: i64) -> Local {
    use std::ffi::{c_char, c_int, c_long};
    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const c_long, tm: *mut Tm) -> *mut Tm;
    }
    let t = secs as c_long;
    let mut tm = Tm { sec: 0, min: 0, hour: 0, mday: 1, mon: 0, year: 70, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null() };
    // SAFETY: localtime_r only writes into `tm`
    let ok = unsafe { !localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return Local::default();
    }
    Local { year: tm.year + 1900, month: tm.mon as u32 + 1, day: tm.mday as u32, hour: tm.hour as u32, min: tm.min as u32, sec: tm.sec as u32 }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "Sep 23 2026 18:23" (nest's modified-time format).
pub fn stamp(secs: i64) -> String {
    let l = local(secs);
    format!("{} {:02} {} {:02}:{:02}", MONTHS[(l.month.clamp(1, 12) - 1) as usize], l.day, l.year, l.hour, l.min)
}

/// "20:41:54"
pub fn hms(secs: i64) -> String {
    let l = local(secs);
    format!("{:02}:{:02}:{:02}", l.hour, l.min, l.sec)
}

/// "20260923-184214" (nest's note id prefix).
pub fn compact(secs: i64) -> String {
    let l = local(secs);
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", l.year, l.month, l.day, l.hour, l.min, l.sec)
}

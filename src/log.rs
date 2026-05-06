//! Stderr logger.
//!
//! `-v` to info, `-vv` to debug, and `-vvv` to trace.
//! Log sites avoid raw secrets. Debug and trace output can still include
//! health data from API payloads.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub const INFO: u8 = 1;
pub const DEBUG: u8 = 2;
pub const TRACE: u8 = 3;

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

pub fn enabled(at: u8) -> bool {
    level() >= at
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::INFO) {
            eprintln!("[info] {}", format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::DEBUG) {
            eprintln!("[debug] {}", format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::TRACE) {
            eprintln!("[trace] {}", format_args!($($arg)*));
        }
    };
}

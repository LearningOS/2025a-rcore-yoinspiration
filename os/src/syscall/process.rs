//! Process management syscalls
use crate::{
    task::{exit_current_and_run_next, suspend_current_and_run_next},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

use crate::task::{get_syscall_count, inc_syscall_count};

/// Trace syscall: read memory, write memory, or count syscalls
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        0 => {
            // Read: read one byte from address id
            unsafe {
                let addr = id as *const u8;
                *addr as isize
            }
        }
        1 => {
            // Write: write data (as u8) to address id
            unsafe {
                let addr = id as *mut u8;
                *addr = data as u8;
            }
            0
        }
        2 => {
            // Count: get syscall count for syscall id
            // Note: This call itself should also be counted
            // First count this sys_trace call before querying
            inc_syscall_count(410); // SYSCALL_TRACE = 410
            let count = get_syscall_count(id);
            count as isize
        }
        _ => -1,
    }
}

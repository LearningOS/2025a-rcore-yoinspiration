//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{
    translated_byte_buffer, MapPermission, PageTable, PTEFlags, VirtAddr, VirtPageNum,
};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next,
    suspend_current_and_run_next, with_current_memory_set,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// Get time with second and microsecond
/// Handle case where TimeVal might be split across two pages
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let token = current_user_token();
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    // Copy TimeVal struct through page table
    let mut buffers = translated_byte_buffer(token, ts as *const u8, core::mem::size_of::<TimeVal>());
    let time_val_bytes = unsafe {
        core::slice::from_raw_parts(&time_val as *const TimeVal as *const u8, core::mem::size_of::<TimeVal>())
    };

    let mut bytes_written = 0;
    for buffer in &mut buffers {
        let copy_len = buffer.len().min(time_val_bytes.len() - bytes_written);
        if copy_len > 0 {
            buffer[..copy_len].copy_from_slice(&time_val_bytes[bytes_written..bytes_written + copy_len]);
            bytes_written += copy_len;
        }
        if bytes_written >= time_val_bytes.len() {
            break;
        }
    }

    0
}

/// Trace syscall: read memory, write memory, or count syscalls
/// Now with address space checking
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    let token = current_user_token();
    let page_table = PageTable::from_token(token);

    match trace_request {
        0 => {
            // Read: read one byte from address id
            // Check if address is valid and readable with U flag
            let va = VirtAddr::from(id);
            let vpn: VirtPageNum = va.floor();
            if let Some(pte) = page_table.translate(vpn) {
                if !pte.is_valid() || !pte.readable() {
                    return -1;
                }
                // Check if page has U flag (user accessible)
                let flags = pte.flags();
                if !flags.contains(PTEFlags::U) {
                    return -1;
                }
                // Read the byte
                let ppn = pte.ppn();
                let offset = va.page_offset();
                unsafe {
                    let byte_ptr = (ppn.get_bytes_array().as_ptr() as usize + offset) as *const u8;
                    *byte_ptr as isize
                }
            } else {
                -1
            }
        }
        1 => {
            // Write: write data (as u8) to address id
            // Check if address is valid, writable, and has U flag
            let va = VirtAddr::from(id);
            let vpn: VirtPageNum = va.floor();
            if let Some(pte) = page_table.translate(vpn) {
                if !pte.is_valid() || !pte.writable() {
                    return -1;
                }
                // Check if page has U flag (user accessible)
                let flags = pte.flags();
                if !flags.contains(PTEFlags::U) {
                    return -1;
                }
                // Write the byte
                let ppn = pte.ppn();
                let offset = va.page_offset();
                unsafe {
                    let byte_ptr = (ppn.get_bytes_array().as_mut_ptr() as usize + offset) as *mut u8;
                    *byte_ptr = data as u8;
                }
                0
            } else {
                -1
            }
        }
        2 => {
            // Count: get syscall count
            // In ch4, syscall counting is not required for trace syscall
            // Return -1 to indicate this feature is not implemented in ch4
            // (ch4 exercise only requires read/write memory, not counting)
            -1
        }
        _ => -1,
    }
}

/// Implement mmap: map physical memory to virtual memory
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    // Check alignment
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    
    // Check prot flags: bit 0=R, bit 1=W, bit 2=X, other bits must be 0
    if prot & !0x7 != 0 {
        return -1;
    }
    if prot & 0x7 == 0 {
        return -1; // At least one permission required
    }
    
    // Convert prot to MapPermission
    let mut map_perm = MapPermission::U; // Always user accessible
    if prot & 0x1 != 0 {
        map_perm |= MapPermission::R;
    }
    if prot & 0x2 != 0 {
        map_perm |= MapPermission::W;
    }
    if prot & 0x4 != 0 {
        map_perm |= MapPermission::X;
    }
    
    let start_va = VirtAddr::from(start);
    
    // Round up len to page size
    let rounded_len = ((len + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;
    let end_va_rounded = VirtAddr::from(start + rounded_len);
    
    // Check if any pages in the range are already mapped
    let token = current_user_token();
    let page_table = PageTable::from_token(token);
    let start_vpn: VirtPageNum = start_va.floor();
    let end_vpn: VirtPageNum = end_va_rounded.ceil();
    
    let mut current_vpn = start_vpn;
    while current_vpn.0 < end_vpn.0 {
        if let Some(pte) = page_table.translate(current_vpn) {
            if pte.is_valid() {
                return -1; // Page already mapped
            }
        }
        current_vpn.0 += 1;
    }
    
    // Insert framed area
    with_current_memory_set(|memory_set| {
        // Check again with exclusive access to memory_set
        let mut current_vpn = start_vpn;
        while current_vpn.0 < end_vpn.0 {
            if let Some(pte) = memory_set.translate(current_vpn) {
                if pte.is_valid() {
                    return -1; // Page already mapped
                }
            }
            current_vpn.0 += 1;
        }
        
        // All checks passed, insert the area
        memory_set.insert_framed_area(start_va, end_va_rounded, map_perm);
        0
    })
}

/// Implement munmap: unmap virtual memory
pub fn sys_munmap(start: usize, len: usize) -> isize {
    let start_va = VirtAddr::from(start);
    
    // Round up len to page size
    let rounded_len = ((len + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;
    let end_va_rounded = VirtAddr::from(start + rounded_len);
    
    // Check if all pages in the range are mapped
    let token = current_user_token();
    let page_table = PageTable::from_token(token);
    let start_vpn: VirtPageNum = start_va.floor();
    let end_vpn: VirtPageNum = end_va_rounded.ceil();
    
    // First check: verify all pages are mapped
    let mut current_vpn = start_vpn;
    while current_vpn.0 < end_vpn.0 {
        if let Some(pte) = page_table.translate(current_vpn) {
            if !pte.is_valid() {
                return -1; // Page not mapped
            }
        } else {
            return -1; // Page table entry doesn't exist
        }
        current_vpn.0 += 1;
    }
    
    // All pages are mapped, now unmap them through page table
    // We need to access the page table directly to unmap
    // Since page_table is private in MemorySet, we use PageTable::from_token
    // to get a mutable reference to the page table
    with_current_memory_set(|memory_set| {
        // Create a mutable PageTable reference using the token
        let token = memory_set.token();
        let mut page_table = PageTable::from_token(token);
        
        let mut current_vpn = start_vpn;
        while current_vpn.0 < end_vpn.0 {
            // Unmap is safe because we already verified all pages are valid
            page_table.unmap(current_vpn);
            current_vpn.0 += 1;
        }
        0
    })
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    loader::get_app_data_by_name,
    mm::{translated_refmut, translated_str},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, TaskControlBlock,
    },
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// Get time with second and microsecond
/// Handle case where TimeVal might be split across two pages
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let token = current_user_token();
    let us = crate::timer::get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    // Copy TimeVal struct through page table
    let mut buffers = crate::mm::translated_byte_buffer(token, ts as *const u8, core::mem::size_of::<TimeVal>());
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

/// Implement mmap: map physical memory to virtual memory
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    use crate::config::PAGE_SIZE;
    use crate::mm::{MapPermission, PageTable, VirtAddr, VirtPageNum};
    use crate::task::current_user_token;

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
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    
    // Check again with exclusive access to memory_set
    let mut current_vpn = start_vpn;
    while current_vpn.0 < end_vpn.0 {
        if let Some(pte) = inner.memory_set.translate(current_vpn) {
            if pte.is_valid() {
                return -1; // Page already mapped
            }
        }
        current_vpn.0 += 1;
    }
    
    // All checks passed, insert the area
    inner.memory_set.insert_framed_area(start_va, end_va_rounded, map_perm);
    0
}

/// Implement munmap: unmap virtual memory
pub fn sys_munmap(start: usize, len: usize) -> isize {
    use crate::config::PAGE_SIZE;
    use crate::mm::{PageTable, VirtAddr, VirtPageNum};
    use crate::task::current_user_token;

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
    
    // All pages are mapped, now unmap them
    // Use PageTable::from_token to get access to the page table
    // Even though it's a temporary object, find_pte accesses physical memory directly
    // so the modifications will be reflected in the actual page table
    let mut page_table = PageTable::from_token(token);
    
    let mut current_vpn = start_vpn;
    while current_vpn.0 < end_vpn.0 {
        // Unmap is safe because we already verified all pages are valid
        page_table.unmap(current_vpn);
        current_vpn.0 += 1;
    }
    
    // Note: According to the exercise, we don't need to handle area removal on errors
    // The pages are unmapped, which is the main requirement
    
    0
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// Implement spawn: create a new process to execute target program
/// Unlike fork+exec, spawn does NOT copy parent's address space
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path_str = translated_str(token, path);
    
    // Get application data by name
    if let Some(data) = get_app_data_by_name(path_str.as_str()) {
        let current_task = current_task().unwrap();
        
        // Create a new task with the specified program
        // Unlike fork, we don't copy the address space
        let new_task = Arc::new(TaskControlBlock::new(data));
        let new_pid = new_task.pid.0;
        
        // Set parent-child relationship
        {
            let mut parent_inner = current_task.inner_exclusive_access();
            parent_inner.children.push(new_task.clone());
        }
        {
            let mut child_inner = new_task.inner_exclusive_access();
            child_inner.parent = Some(Arc::downgrade(&current_task));
        }
        
        // Add new task to scheduler
        add_task(new_task);
        
        new_pid as isize
    } else {
        -1 // Invalid filename
    }
}

/// Set task priority for stride scheduling
pub fn sys_set_priority(prio: isize) -> isize {
    use crate::config::BIG_STRIDE;
    
    trace!("kernel:pid[{}] sys_set_priority [{}]", current_task().unwrap().pid.0, prio);
    
    // Priority must be >= 2
    if prio < 2 {
        return -1;
    }
    
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    
    // Update priority and pass
    inner.priority = prio as usize;
    inner.pass = BIG_STRIDE / inner.priority;
    
    prio
}

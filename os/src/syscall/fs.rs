//! File and filesystem-related syscalls
use crate::fs::{open_file, OpenFlags, Stat, StatMode, ROOT_INODE};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
use crate::task::{current_task, current_user_token};
use easy_fs::DiskInodeType;

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_write", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_read", current_task().unwrap().pid.0);
    let token = current_user_token();
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!("kernel:pid[{}] sys_open", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = open_file(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = task.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    trace!("kernel:pid[{}] sys_close", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

/// Get file status
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!("kernel:pid[{}] sys_fstat", current_task().unwrap().pid.0);
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        // Get the inode through File trait method
        // For OSInode, this will return Some, for stdin/stdout, it will return None
        let file_clone = file.clone();
        drop(inner);
        
        // Try to get the inode
        if let Some(inode) = file_clone.get_inode() {
            // Get inode information
            let inode_id = inode.inode_id() as u64;
            let nlink = inode.nlink();
            let inode_type = inode.inode_type();
            
            // Convert DiskInodeType to StatMode
            let mode = if inode_type == DiskInodeType::Directory {
                StatMode::DIR
            } else {
                StatMode::FILE
            };
            
            // Fill Stat structure
            let stat = Stat {
                dev: 0, // Device ID, always 0 as specified
                ino: inode_id,
                mode,
                nlink,
                pad: [0; 7],
            };
            
            // Write Stat to user space
            let token = current_user_token();
            *translated_refmut(token, st) = stat;
            0
        } else {
            // Not an OSInode (e.g., stdin/stdout), return error
            -1
        }
    } else {
        -1
    }
}

/// Create a hard link
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_linkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let old_path = translated_str(token, old_name);
    let new_path = translated_str(token, new_name);
    
    // Check if old_path and new_path are the same
    if old_path == new_path {
        return -1;
    }
    
    // Find the target inode
    if let Some(target_inode) = ROOT_INODE.find(old_path.as_str()) {
        // Check if new_path already exists
        if ROOT_INODE.find(new_path.as_str()).is_some() {
            return -1; // New path already exists
        }
        
        // Create the hard link
        if ROOT_INODE.link_at(new_path.as_str(), target_inode) {
            0
        } else {
            -1
        }
    } else {
        -1 // Old path not found
    }
}

/// Remove a hard link
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_unlinkat", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, name);
    
    // Check if file exists
    if ROOT_INODE.find(path.as_str()).is_none() {
        return -1; // File not found
    }
    
    // Remove the hard link
    if ROOT_INODE.unlink_at(path.as_str()) {
        0
    } else {
        -1
    }
}

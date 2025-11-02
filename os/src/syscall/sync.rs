use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{block_current_and_run_next, current_process, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Deadlock detection return value
const DEADLOCK_RETURN: isize = -0xDEAD;

/// Check if deadlock will occur using banker's algorithm
/// Returns true if safe (no deadlock), false if unsafe (deadlock detected)
/// This function assumes that Need[tid][resource_id] has already been incremented
fn check_deadlock_internal(
    process_inner: &mut crate::task::ProcessControlBlockInner,
    tid: usize,
    resource_id: usize,
    _resource_type: ResourceType,
) -> bool {
    
    // Get resource counts
    let mutex_count = process_inner.mutex_list.len();
    let semaphore_count = process_inner.semaphore_list.len();
    let resource_count = mutex_count + semaphore_count;
    let thread_count = process_inner.tasks.len();
    
    // Initialize or expand matrices if needed
    if process_inner.allocation.len() < thread_count {
        process_inner.allocation.resize(thread_count, Vec::new());
    }
    if process_inner.need.len() < thread_count {
        process_inner.need.resize(thread_count, Vec::new());
    }
    for i in 0..thread_count {
        if process_inner.allocation[i].len() < resource_count {
            process_inner.allocation[i].resize(resource_count, 0);
        }
        if process_inner.need[i].len() < resource_count {
            process_inner.need[i].resize(resource_count, 0);
        }
    }
    
    // Calculate Available vector
    let mut available = Vec::new();
    available.resize(resource_count, 0);
    
    // For mutexes: max count is 1
    for i in 0..mutex_count {
        let mut total_allocated = 0;
        for j in 0..thread_count {
            total_allocated += process_inner.allocation[j][i];
        }
        available[i] = 1usize.saturating_sub(total_allocated);
    }
    
    // For semaphores: use stored res_count (max resources when created)
    for i in 0..semaphore_count {
        if i < process_inner.semaphore_res_counts.len() {
            let res_count = process_inner.semaphore_res_counts[i];
            let mut total_allocated = 0;
            let res_idx = mutex_count + i;
            for j in 0..thread_count {
                total_allocated += process_inner.allocation[j][res_idx];
            }
            available[res_idx] = res_count.saturating_sub(total_allocated);
        }
    }
    
    // Temporarily update allocation and available for this request
    let old_allocation = process_inner.allocation[tid][resource_id];
    
    // Simulate allocation: increment by 1
    process_inner.allocation[tid][resource_id] += 1;
    
    // Update available
    if available[resource_id] > 0 {
        available[resource_id] -= 1;
    }
    
    // Run banker's algorithm
    let mut work = available.clone();
    let mut finish = Vec::new();
    finish.resize(thread_count, false);
    
    loop {
        let mut found = false;
        for i in 0..thread_count {
            if !finish[i] {
                // Check if Need[i] <= Work
                let mut can_satisfy = true;
                for j in 0..resource_count {
                    if process_inner.need[i][j] > work[j] {
                        can_satisfy = false;
                        break;
                    }
                }
                if can_satisfy {
                    // Work = Work + Allocation[i]
                    for j in 0..resource_count {
                        work[j] += process_inner.allocation[i][j];
                    }
                    finish[i] = true;
                    found = true;
                }
            }
        }
        if !found {
            break;
        }
    }
    
    // Check if all threads can finish
    let is_safe = finish.iter().all(|&f| f);
    
    // Restore old allocation value
    process_inner.allocation[tid][resource_id] = old_allocation;
    
    is_safe
}

/// Resource type for deadlock detection
enum ResourceType {
    Mutex,
    Semaphore,
}
/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}
/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
    
    // Deadlock detection
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.deadlock_detection_enabled {
        // Update Need: thread needs this mutex
        let resource_id = mutex_id;
        let thread_count = process_inner.tasks.len();
        let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
        
        // Initialize matrices if needed
        if process_inner.allocation.len() < thread_count {
            process_inner.allocation.resize(thread_count, Vec::new());
        }
        if process_inner.need.len() < thread_count {
            process_inner.need.resize(thread_count, Vec::new());
        }
        for i in 0..thread_count {
            if process_inner.allocation[i].len() < resource_count {
                process_inner.allocation[i].resize(resource_count, 0);
            }
            if process_inner.need[i].len() < resource_count {
                process_inner.need[i].resize(resource_count, 0);
            }
        }
        
        // Mark that this thread needs this mutex
        process_inner.need[tid][resource_id] += 1;
        
        // Check if safe
        let is_safe = check_deadlock_internal(&mut process_inner, tid, resource_id, ResourceType::Mutex);
        
        if !is_safe {
            // Restore Need
            process_inner.need[tid][resource_id] -= 1;
            drop(process_inner);
            return DEADLOCK_RETURN;
        }
        
        // Safe: update allocation (will be done after successful lock)
        // For now, we just mark the need, allocation will be updated after lock succeeds
        drop(process_inner);
    } else {
        drop(process_inner);
    }
    
    let mutex = {
        let process_inner = process.inner_exclusive_access();
        Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap())
    };
    
    mutex.lock();
    
    // After successful lock, update allocation
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.deadlock_detection_enabled {
        let resource_id = mutex_id;
        let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
        
        // Ensure matrices are initialized
        if process_inner.allocation[tid].len() < resource_count {
            process_inner.allocation[tid].resize(resource_count, 0);
        }
        if process_inner.need[tid].len() < resource_count {
            process_inner.need[tid].resize(resource_count, 0);
        }
        
        // Update allocation: thread got the mutex
        process_inner.allocation[tid][resource_id] += 1;
        // Decrease need
        if process_inner.need[tid][resource_id] > 0 {
            process_inner.need[tid][resource_id] -= 1;
        }
    }
    drop(process_inner);
    
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
    
    let mutex = {
        let mut process_inner = process.inner_exclusive_access();
        // Update allocation: release the mutex
        if process_inner.deadlock_detection_enabled {
            let resource_id = mutex_id;
            let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
            
            // Ensure matrices are initialized
            if process_inner.allocation.len() > tid && process_inner.allocation[tid].len() >= resource_count {
                // Decrease allocation
                if process_inner.allocation[tid][resource_id] > 0 {
                    process_inner.allocation[tid][resource_id] -= 1;
                }
            }
        }
        Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap())
    };
    
    mutex.unlock();
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        // Store the resource count for deadlock detection
        if process_inner.semaphore_res_counts.len() <= id {
            process_inner.semaphore_res_counts.resize(id + 1, 0);
        }
        process_inner.semaphore_res_counts[id] = res_count;
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_res_counts.push(res_count);
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
    
    let sem = {
        let mut process_inner = process.inner_exclusive_access();
        // Update allocation: release the semaphore
        if process_inner.deadlock_detection_enabled {
            let mutex_count = process_inner.mutex_list.len();
            let resource_id = mutex_count + sem_id;
            let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
            
            // Ensure matrices are initialized
            if process_inner.allocation.len() > tid && process_inner.allocation[tid].len() >= resource_count {
                // Decrease allocation
                if process_inner.allocation[tid][resource_id] > 0 {
                    process_inner.allocation[tid][resource_id] -= 1;
                }
            }
        }
        Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap())
    };
    
    sem.up();
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let task = current_task().unwrap();
    let tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
    
    // Deadlock detection
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.deadlock_detection_enabled {
        // Update Need: thread needs this semaphore
        let mutex_count = process_inner.mutex_list.len();
        let resource_id = mutex_count + sem_id;
        let thread_count = process_inner.tasks.len();
        let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
        
        // Initialize matrices if needed
        if process_inner.allocation.len() < thread_count {
            process_inner.allocation.resize(thread_count, Vec::new());
        }
        if process_inner.need.len() < thread_count {
            process_inner.need.resize(thread_count, Vec::new());
        }
        for i in 0..thread_count {
            if process_inner.allocation[i].len() < resource_count {
                process_inner.allocation[i].resize(resource_count, 0);
            }
            if process_inner.need[i].len() < resource_count {
                process_inner.need[i].resize(resource_count, 0);
            }
        }
        
        // Mark that this thread needs this semaphore
        process_inner.need[tid][resource_id] += 1;
        
        // Check if safe
        let is_safe = check_deadlock_internal(&mut process_inner, tid, resource_id, ResourceType::Semaphore);
        
        if !is_safe {
            // Restore Need
            process_inner.need[tid][resource_id] -= 1;
            drop(process_inner);
            return DEADLOCK_RETURN;
        }
        
        drop(process_inner);
    } else {
        drop(process_inner);
    }
    
    let sem = {
        let process_inner = process.inner_exclusive_access();
        Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap())
    };
    
    sem.down();
    
    // After successful down, update allocation
    let mut process_inner = process.inner_exclusive_access();
    if process_inner.deadlock_detection_enabled {
        let mutex_count = process_inner.mutex_list.len();
        let resource_id = mutex_count + sem_id;
        let resource_count = process_inner.mutex_list.len() + process_inner.semaphore_list.len();
        
        // Ensure matrices are initialized
        if process_inner.allocation[tid].len() < resource_count {
            process_inner.allocation[tid].resize(resource_count, 0);
        }
        if process_inner.need[tid].len() < resource_count {
            process_inner.need[tid].resize(resource_count, 0);
        }
        
        // Update allocation: thread got the semaphore
        process_inner.allocation[tid][resource_id] += 1;
        // Decrease need
        if process_inner.need[tid][resource_id] > 0 {
            process_inner.need[tid][resource_id] -= 1;
        }
    }
    drop(process_inner);
    
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// Enable or disable deadlock detection for the current process
pub fn sys_enable_deadlock_detect(is_enable: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_enable_deadlock_detect is_enable={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        is_enable
    );
    
    if is_enable > 1 {
        return -1; // Invalid parameter
    }
    
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.deadlock_detection_enabled = is_enable == 1;
    
    // If disabling, clear allocation and need matrices
    if is_enable == 0 {
        process_inner.allocation.clear();
        process_inner.need.clear();
    }
    
    0
}

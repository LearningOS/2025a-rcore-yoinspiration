use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DiskInode, DiskInodeType,
    EasyFileSystem,
};
use crate::layout::{DirEntry, DIRENT_SZ};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
    inode_id: u32,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
        inode_id: u32,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
            inode_id,
        }
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some(dirent.inode_id() as u32);
            }
        }
        None
    }
    /// Find inode under current inode by name
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                    inode_id,
                ))
            })
        })
    }
    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(new_size as u32, root_inode, &mut fs);
            // write dirent
            let dirent = DirEntry::new(name, new_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
            new_inode_id,
        )))
        // release efs lock automatically by compiler
    }
    /// List inodes under current inode
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }
    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }
    /// Write data to current inode
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
    /// Create a hard link from current directory to target inode
    /// link_name: name of the new link
    /// target_inode: the target inode to link to
    /// Returns true on success, false on error
    pub fn link_at(&self, link_name: &str, target_inode: Arc<Inode>) -> bool {
        // Check if link_name already exists
        if self.find(link_name).is_some() {
            return false;
        }
        let mut fs = self.fs.lock();
        // Get target inode_id
        let target_inode_id = target_inode.inode_id;
        // Increase nlink for target inode
        target_inode.modify_disk_inode(|disk_inode| {
            disk_inode.nlink += 1;
        });
        // Add directory entry to current directory
        self.modify_disk_inode(|parent_disk_inode| {
            // Assert it is a directory
            assert!(parent_disk_inode.is_dir());
            // Append file in the dirent
            let file_count = (parent_disk_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // Increase size
            self.increase_size(new_size as u32, parent_disk_inode, &mut fs);
            // Write dirent
            let dirent = DirEntry::new(link_name, target_inode_id);
            parent_disk_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });
        block_cache_sync_all();
        true
    }
    /// Remove a hard link from current directory
    /// link_name: name of the link to remove
    /// Returns true on success, false on error (e.g., file not found)
    pub fn unlink_at(&self, link_name: &str) -> bool {
        let mut fs = self.fs.lock();
        // Find the inode_id of the file to unlink
        let target_inode_id = self.read_disk_inode(|parent_disk_inode| {
            self.find_inode_id(link_name, parent_disk_inode)
        });
        if target_inode_id.is_none() {
            return false;
        }
        let target_inode_id = target_inode_id.unwrap();
        // Remove directory entry from current directory
        self.modify_disk_inode(|parent_disk_inode| {
            // Assert it is a directory
            assert!(parent_disk_inode.is_dir());
            let file_count = (parent_disk_inode.size as usize) / DIRENT_SZ;
            // Find the entry to remove
            let mut dirent = DirEntry::empty();
            let mut found_idx = None;
            for i in 0..file_count {
                assert_eq!(
                    parent_disk_inode.read_at(
                        i * DIRENT_SZ,
                        dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
                    DIRENT_SZ,
                );
                if dirent.name() == link_name {
                    found_idx = Some(i);
                    break;
                }
            }
            if found_idx.is_none() {
                return;
            }
            let idx = found_idx.unwrap();
            // Move subsequent entries forward
            let mut next_dirent = DirEntry::empty();
            for i in idx + 1..file_count {
                assert_eq!(
                    parent_disk_inode.read_at(
                        i * DIRENT_SZ,
                        next_dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
                    DIRENT_SZ,
                );
                parent_disk_inode.write_at(
                    (i - 1) * DIRENT_SZ,
                    next_dirent.as_bytes(),
                    &self.block_device,
                );
            }
            // Decrease size
            let new_size = (file_count - 1) * DIRENT_SZ;
            // Note: We don't shrink the directory size for simplicity
            // The size field can remain larger, but the actual used size is smaller
            parent_disk_inode.size = new_size as u32;
        });
        // Decrease nlink for target inode
        let (target_block_id, target_block_offset) = fs.get_disk_inode_pos(target_inode_id);
        let should_dealloc = get_block_cache(target_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(target_block_offset, |target_disk_inode: &mut DiskInode| {
                target_disk_inode.nlink -= 1;
                // If nlink reaches 0, deallocate the inode and its data blocks
                if target_disk_inode.nlink == 0 {
                    let data_blocks_dealloc = target_disk_inode.clear_size(&self.block_device);
                    // Deallocate data blocks
                    for data_block in data_blocks_dealloc.into_iter() {
                        fs.dealloc_data(data_block);
                    }
                    true
                } else {
                    false
                }
            });
        // Deallocate the inode if nlink reached 0
        if should_dealloc {
            fs.dealloc_inode(target_inode_id);
        }
        block_cache_sync_all();
        true
    }
    /// Get inode id
    pub fn inode_id(&self) -> u32 {
        self.inode_id
    }
    /// Get nlink count
    pub fn nlink(&self) -> u32 {
        self.read_disk_inode(|disk_inode| disk_inode.nlink)
    }
    /// Get inode type (for Stat mode)
    pub fn inode_type(&self) -> DiskInodeType {
        self.read_disk_inode(|disk_inode| disk_inode.get_type())
    }
}

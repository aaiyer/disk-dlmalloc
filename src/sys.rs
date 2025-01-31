use std::path::Path;
use crate::Allocator;
use core::ptr;
use std::fs::OpenOptions;
use std::sync::Mutex;
use memmap2::MmapMut;

pub struct System {
    inner: Mutex<Inner>,
    page_size: usize,
}

struct Inner {
    mmap: MmapMut,
    total_size: usize,
    offset: usize,
}

impl System {
    pub fn new<P: AsRef<Path>>(file_path: P, total_size: usize) -> System {
        let file_path = file_path.as_ref().to_path_buf();
        let file = match OpenOptions::new()
          .read(true)
          .write(true)
          .create(true)
          .truncate(true)
          .open(&file_path) {
            Ok(file) => file,
            Err(err) => panic!("Could not open file {}: {:?}", file_path.display(), err),
        };
        if let Err(err) = file.set_len(total_size as u64) {
            panic!("Could not set file size {}: {:?}", file_path.display(), err);
        }
        let mmap: MmapMut = unsafe {
            match MmapMut::map_mut(&file) {
                Ok(mmap) => mmap,
                Err(err) => panic!("Could not mmap file {}: {:?}", file_path.display(), err),
            }
        };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        System {
            inner: Mutex::new(Inner {
                mmap,
                total_size,
                offset: 0,
            }),
            page_size,
        }
    }
}

unsafe impl Allocator for System {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let mut inner = self.inner.lock().unwrap();
        if inner.offset + size > inner.total_size {
            return (ptr::null_mut(), 0, 0);
        }
        let ptr = unsafe { inner.mmap.as_mut_ptr().add(inner.offset) };
        inner.offset += size;
        (ptr, size, 0)
    }

    fn remap(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool) -> *mut u8 {
        ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
        false
    }

    fn free(&self, _ptr: *mut u8, _size: usize) -> bool {
        false
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        self.page_size
    }
}

#[cfg(feature = "global")]
static mut LOCK: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;

#[cfg(feature = "global")]
pub fn acquire_global_lock() {
    unsafe { assert_eq!(libc::pthread_mutex_lock(ptr::addr_of_mut!(LOCK)), 0) }
}

#[cfg(feature = "global")]
pub fn release_global_lock() {
    unsafe { assert_eq!(libc::pthread_mutex_unlock(ptr::addr_of_mut!(LOCK)), 0) }
}

#[cfg(feature = "global")]
/// allows the allocator to remain unsable in the child process,
/// after a call to `fork(2)`
///
/// #Safety
///
/// if used, this function must be called,
/// before any allocations are made with the global allocator.
pub unsafe fn enable_alloc_after_fork() {
    // atfork must only be called once, to avoid a deadlock,
    // where the handler attempts to acquire the global lock twice
    static mut FORK_PROTECTED: bool = false;

    unsafe extern "C" fn _acquire_global_lock() {
        acquire_global_lock()
    }

    unsafe extern "C" fn _release_global_lock() {
        release_global_lock()
    }

    acquire_global_lock();
    // if a process forks,
    // it will acquire the lock before any other thread,
    // protecting it from deadlock,
    // due to the child being created with only the calling thread.
    if !FORK_PROTECTED {
        libc::pthread_atfork(
            Some(_acquire_global_lock),
            Some(_release_global_lock),
            Some(_release_global_lock),
        );
        FORK_PROTECTED = true;
    }
    release_global_lock();
}

use crate::Allocator;
use core::ptr;
use memmap2::{Advice, MmapMut};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

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
    pub fn new<P: AsRef<Path>>(
        file_path: P,
        total_size: usize,
        mem_advise: Option<Advice>,
    ) -> System {
        let file_path = file_path.as_ref().to_path_buf();
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&file_path)
        {
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
        let mem_advise = mem_advise.unwrap_or(Advice::Normal);
        if let Err(err) = mmap.advise(mem_advise) {
            panic!(
                "Could not mem advise mmap for file {}: {:?}",
                file_path.display(),
                err
            );
        }
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

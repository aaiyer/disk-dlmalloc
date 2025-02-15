//! A fork of [dlmalloc-rs] backed by a memory-mapped file, enabling support for datasets
//! exceeding available RAM.
//!
//! The `dlmalloc` allocator is described at
//! <https://gee.cs.oswego.edu/dl/html/malloc.html> and this Rust crate is a straight
//! port of the C code for the allocator into Rust. The implementation is
//! wrapped up in a `Dlmalloc` type and has support for Linux, OSX, and Wasm
//! currently.

#![allow(dead_code)]
#![deny(missing_docs)]
#![feature(allocator_api)]

use core::cmp;
use core::ptr;
use std::alloc::{AllocError, Layout};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use sys::System;

mod dlmalloc;
mod sys;

pub use memmap2::Advice;

/// In order for this crate to efficiently manage memory, it needs a way to communicate with the
/// underlying platform. This `Allocator` trait provides an interface for this communication.
pub unsafe trait SystemAllocator: Send {
    /// Allocates system memory region of at least `size` bytes
    /// Returns a triple of `(base, size, flags)` where `base` is a pointer to the beginning of the
    /// allocated memory region. `size` is the actual size of the region while `flags` specifies
    /// properties of the allocated region. If `EXTERN_BIT` (bit 0) set in flags, then we did not
    /// allocate this segment and so should not try to deallocate or merge with others.
    /// This function can return a `std::ptr::null_mut()` when allocation fails (other values of
    /// the triple will be ignored).
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32);

    /// Remaps system memory region at `ptr` with size `oldsize` to a potential new location with
    /// size `newsize`. `can_move` indicates if the location is allowed to move to a completely new
    /// location, or that it is only allowed to change in size. Returns a pointer to the new
    /// location in memory.
    /// This function can return a `std::ptr::null_mut()` to signal an error.
    fn remap(&self, ptr: *mut u8, oldsize: usize, newsize: usize, can_move: bool) -> *mut u8;

    /// Frees a part of a memory chunk. The original memory chunk starts at `ptr` with size `oldsize`
    /// and is turned into a memory region starting at the same address but with `newsize` bytes.
    /// Returns `true` iff the access memory region could be freed.
    fn free_part(&self, ptr: *mut u8, oldsize: usize, newsize: usize) -> bool;

    /// Frees an entire memory region. Returns `true` iff the operation succeeded. When `false` is
    /// returned, the `dlmalloc` may re-use the location on future allocation requests
    fn free(&self, ptr: *mut u8, size: usize) -> bool;

    /// Indicates if the system can release a part of memory. For the `flags` argument, see
    /// `Allocator::alloc`
    fn can_release_part(&self, flags: u32) -> bool;

    /// Indicates whether newly allocated regions contain zeros.
    fn allocates_zeros(&self) -> bool;

    /// Returns the page size. Must be a power of two
    fn page_size(&self) -> usize;
}

/// An allocator instance
#[derive(Clone)]
pub struct DiskDlmalloc(Arc<Mutex<dlmalloc::Dlmalloc<System>>>);

impl DiskDlmalloc {
    /// Creates a new instance of an allocator
    pub fn new<P: AsRef<Path>>(
        file_path: P,
        total_size: usize,
        mem_advise: Option<Advice>,
    ) -> DiskDlmalloc {
        DiskDlmalloc(Arc::new(Mutex::new(dlmalloc::Dlmalloc::new(System::new(
            file_path, total_size, mem_advise,
        )))))
    }
}

impl DiskDlmalloc {
    /// Allocates `size` bytes with `align` align.
    ///
    /// Returns a null pointer if allocation fails. Returns a valid pointer
    /// otherwise.
    ///
    /// Safety and contracts are largely governed by the `GlobalAlloc::alloc`
    /// method contracts.
    #[inline]
    pub unsafe fn malloc(&self, size: usize, align: usize) -> *mut u8 {
        let mut me = self.0.lock().unwrap();
        if align <= me.malloc_alignment() {
            me.malloc(size)
        } else {
            me.memalign(align, size)
        }
    }

    /// Same as `malloc`, except if the allocation succeeds it's guaranteed to
    /// point to `size` bytes of zeros.
    #[inline]
    pub unsafe fn calloc(&self, size: usize, align: usize) -> *mut u8 {
        let ptr = self.malloc(size, align);
        let me = self.0.lock().unwrap();
        if !ptr.is_null() && me.calloc_must_clear(ptr) {
            ptr::write_bytes(ptr, 0, size);
        }
        ptr
    }

    /// Deallocates a `ptr` with `size` and `align` as the previous request used
    /// to allocate it.
    ///
    /// Safety and contracts are largely governed by the `GlobalAlloc::dealloc`
    /// method contracts.
    #[inline]
    pub unsafe fn free(&self, ptr: *mut u8, size: usize, align: usize) {
        let _ = align;
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr, size);
        me.free(ptr)
    }

    /// Reallocates `ptr`, a previous allocation with `old_size` and
    /// `old_align`, to have `new_size` and the same alignment as before.
    ///
    /// Returns a null pointer if the memory couldn't be reallocated, but `ptr`
    /// is still valid. Returns a valid pointer and frees `ptr` if the request
    /// is satisfied.
    ///
    /// Safety and contracts are largely governed by the `GlobalAlloc::realloc`
    /// method contracts.
    #[inline]
    pub unsafe fn realloc(
        &self,
        ptr: *mut u8,
        old_size: usize,
        old_align: usize,
        new_size: usize,
    ) -> *mut u8 {
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr, old_size);

        if old_align <= me.malloc_alignment() {
            me.realloc(ptr, new_size)
        } else {
            drop(me);
            let res = self.malloc(new_size, old_align);
            if !res.is_null() {
                let size = cmp::min(old_size, new_size);
                ptr::copy_nonoverlapping(ptr, res, size);
                self.free(ptr, old_size, old_align);
            }
            res
        }
    }

    /// If possible, gives memory back to the system if there is unused memory
    /// at the high end of the malloc pool or in unused segments.
    ///
    /// You can call this after freeing large blocks of memory to potentially
    /// reduce the system-level memory requirements of a program. However, it
    /// cannot guarantee to reduce memory. Under some allocation patterns, some
    /// large free blocks of memory will be locked between two used chunks, so
    /// they cannot be given back to the system.
    ///
    /// The `pad` argument represents the amount of free trailing space to
    /// leave untrimmed. If this argument is zero, only the minimum amount of
    /// memory to maintain internal data structures will be left. Non-zero
    /// arguments can be supplied to maintain enough trailing space to service
    /// future expected allocations without having to re-obtain memory from the
    /// system.
    ///
    /// Returns `true` if it actually released any memory, else `false`.
    pub unsafe fn trim(&self, pad: usize) -> bool {
        let mut me = self.0.lock().unwrap();
        me.trim(pad)
    }
}

unsafe impl std::alloc::Allocator for DiskDlmalloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let size = layout.size();
        let align = layout.align();
        let mut me = self.0.lock().unwrap();
        let ptr = if align <= me.malloc_alignment() {
            unsafe { me.malloc(size) }
        } else {
            unsafe { me.memalign(align, size) }
        };
        if ptr.is_null() {
            Err(AllocError)
        } else {
            unsafe {
                Ok(NonNull::slice_from_raw_parts(
                    NonNull::new_unchecked(ptr),
                    size,
                ))
            }
        }
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let size = layout.size();
        let align = layout.align();
        let mut me = self.0.lock().unwrap();
        let ptr = if align <= me.malloc_alignment() {
            unsafe { me.malloc(size) }
        } else {
            unsafe { me.memalign(align, size) }
        };
        if ptr.is_null() {
            return Err(AllocError);
        }
        unsafe {
            if me.calloc_must_clear(ptr) {
                ptr::write_bytes(ptr, 0, size);
            }
        }
        unsafe {
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(ptr),
                size,
            ))
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr.as_ptr(), layout.size());
        me.free(ptr.as_ptr());
    }

    unsafe fn grow(&self,
                   ptr: NonNull<u8>,
                   old_layout: Layout,
                   new_layout: Layout)
                   -> Result<NonNull<[u8]>, AllocError> {
        let old_size = old_layout.size();
        let old_align = old_layout.align();
        let new_size = new_layout.size();
        let new_align = new_layout.align();
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr.as_ptr(), old_size);

        if old_align <= me.malloc_alignment() && new_align <= me.malloc_alignment() {
            let new_ptr = me.realloc(ptr.as_ptr(), new_size);
            if new_ptr.is_null() {
                return Err(AllocError);
            }
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(new_ptr),
                new_size,
            ))
        } else {
            drop(me);
            let res_ptr = self.malloc(new_size, new_align);
            if res_ptr.is_null() {
                return Err(AllocError);
            }
            ptr::copy_nonoverlapping(ptr.as_ptr(), res_ptr, core::cmp::min(old_size, new_size));
            self.free(ptr.as_ptr(), old_size, old_align);
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(res_ptr),
                new_size,
            ))
        }
    }

    unsafe fn grow_zeroed(&self,
                          ptr: NonNull<u8>,
                          old_layout: Layout,
                          new_layout: Layout)
                          -> Result<NonNull<[u8]>, AllocError> {
        let old_size = old_layout.size();
        let old_align = old_layout.align();
        let new_size = new_layout.size();
        let new_align = new_layout.align();
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr.as_ptr(), old_size);

        if old_align <= me.malloc_alignment() && new_align <= me.malloc_alignment() {
            let new_ptr = me.realloc(ptr.as_ptr(), new_size);
            if new_ptr.is_null() {
                return Err(AllocError);
            }
            if new_ptr == ptr.as_ptr() && new_size > old_size {
                ptr::write_bytes(new_ptr.add(old_size), 0, new_size - old_size);
            } else if new_ptr != ptr.as_ptr() && new_size > old_size && me.calloc_must_clear(new_ptr) {
                ptr::copy_nonoverlapping(ptr.as_ptr(), new_ptr, old_size);
                ptr::write_bytes(new_ptr.add(old_size), 0, new_size - old_size);
            }
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(new_ptr),
                new_size,
            ))
        } else {
            drop(me);
            let res_ptr = self.malloc(new_size, new_align);
            if res_ptr.is_null() {
                return Err(AllocError);
            }
            ptr::copy_nonoverlapping(ptr.as_ptr(), res_ptr, core::cmp::min(old_size, new_size));
            if new_size > old_size {
                ptr::write_bytes(res_ptr.add(old_size), 0, new_size - old_size);
            }
            self.free(ptr.as_ptr(), old_size, old_align);
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(res_ptr),
                new_size,
            ))
        }
    }

    unsafe fn shrink(&self,
                     ptr: NonNull<u8>,
                     old_layout: Layout,
                     new_layout: Layout)
                     -> Result<NonNull<[u8]>, AllocError> {
        let old_size = old_layout.size();
        let old_align = old_layout.align();
        let new_size = new_layout.size();
        let new_align = new_layout.align();
        let mut me = self.0.lock().unwrap();
        me.validate_size(ptr.as_ptr(), old_size);

        if old_align <= me.malloc_alignment() && new_align <= me.malloc_alignment() {
            let new_ptr = me.realloc(ptr.as_ptr(), new_size);
            if new_ptr.is_null() {
                return Err(AllocError);
            }
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(new_ptr),
                new_size,
            ))
        } else {
            drop(me);
            let res_ptr = self.malloc(new_size, new_align);
            if res_ptr.is_null() {
                return Err(AllocError);
            }
            ptr::copy_nonoverlapping(ptr.as_ptr(), res_ptr, core::cmp::min(old_size, new_size));
            self.free(ptr.as_ptr(), old_size, old_align);
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(res_ptr),
                new_size,
            ))
        }
    }

    fn by_ref(&self) -> &Self {
        self
    }
}

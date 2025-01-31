use arbitrary::Unstructured;
use disk_dlmalloc::DiskDlmalloc;
use rand::{rngs::SmallRng, RngCore, SeedableRng};
use tempfile::NamedTempFile;

#[test]
fn smoke() {
    let temp_file = NamedTempFile::new().unwrap();
    let temp_file_path = temp_file.path();
    let mut a = DiskDlmalloc::new(&temp_file_path, 10485760);
    unsafe {
        let ptr = a.malloc(1, 1);
        assert!(!ptr.is_null());
        *ptr = 9;
        assert_eq!(*ptr, 9);
        a.free(ptr, 1, 1);

        let ptr = a.malloc(1, 1);
        assert!(!ptr.is_null());
        *ptr = 10;
        assert_eq!(*ptr, 10);
        a.free(ptr, 1, 1);
    }
}

#[path = "../fuzz/src/lib.rs"]
mod fuzz;

#[test]
fn stress() {
    let mut rng = SmallRng::seed_from_u64(0);
    let mut buf = vec![0; 4096];
    let iters = if cfg!(miri) { 5 } else { 2000 };
    for _ in 0..iters {
        rng.fill_bytes(&mut buf);
        let mut u = Unstructured::new(&buf);
        let _ = fuzz::run(&mut u);
    }
}

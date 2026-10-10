use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

pub struct TrackingAllocator;
fn track(bytes: usize) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let _ = CALLS.try_with(|value| value.set(value.get() + 1));
        let _ = BYTES.try_with(|value| value.set(value.get() + bytes));
    }
}
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        track(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        track(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        track(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[derive(Debug)]
pub struct Allocations {
    pub calls: usize,
    pub bytes: usize,
}
pub fn measure<T>(operation: impl FnOnce() -> T) -> (T, Allocations) {
    assert!(
        !ACTIVE.with(Cell::get),
        "allocation measurement cannot nest"
    );
    CALLS.with(|value| value.set(0));
    BYTES.with(|value| value.set(0));
    ACTIVE.with(|value| value.set(true));
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            ACTIVE.with(|value| value.set(false));
        }
    }
    let stop = Stop;
    let output = operation();
    drop(stop);
    (
        output,
        Allocations {
            calls: CALLS.with(Cell::get),
            bytes: BYTES.with(Cell::get),
        },
    )
}

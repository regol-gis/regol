//! Optional hint for sources that cache sparse relation and membership reads.
use std::cell::Cell;

thread_local! { static SPARSE: Cell<bool> = const { Cell::new(false) }; }

pub fn sparse() -> bool {
    SPARSE.get()
}

pub fn with_relation_reads<T>(read: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            SPARSE.set(self.0);
        }
    }
    let _restore = Restore(SPARSE.replace(true));
    read()
}

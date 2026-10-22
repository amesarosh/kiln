//! The typed password, kept in one fixed buffer that is wiped when cleared or dropped, so no
//! stray copies are left behind in freed memory.

use std::sync::atomic::{Ordering, compiler_fence};

/// Longest password accepted, in bytes. The buffer never grows, so it is never moved.
const MAX: usize = 1024;

pub struct Password(String);

impl Password {
    pub fn new() -> Self {
        let s = String::with_capacity(MAX);
        // Keep it out of swap. Not fatal if the memory limit says no.
        // SAFETY: locks pages of our own live allocation.
        unsafe { libc::mlock(s.as_ptr().cast(), s.capacity()) };
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Characters typed (what the dots show).
    pub fn chars(&self) -> usize {
        self.0.chars().count()
    }

    /// Add typed text; ignored once the buffer is full.
    pub fn push(&mut self, text: &str) {
        if self.0.len() + text.len() <= self.0.capacity() {
            self.0.push_str(text);
        }
    }

    pub fn pop(&mut self) {
        let old = self.0.len();
        self.0.pop();
        self.wipe_from(self.0.len(), old);
    }

    pub fn clear(&mut self) {
        let old = self.0.len();
        self.0.clear();
        self.wipe_from(0, old);
    }

    /// Hand the password over, leaving an empty one in its place.
    pub fn take(&mut self) -> Password {
        std::mem::replace(self, Password::new())
    }

    /// Zero bytes `from..to` of the buffer, which lie past the string's end.
    fn wipe_from(&mut self, from: usize, to: usize) {
        let ptr = self.0.as_ptr() as *mut u8;
        for i in from..to.min(self.0.capacity()) {
            // SAFETY: inside our allocation; volatile so the writes aren't optimized away.
            unsafe { ptr.add(i).write_volatile(0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        self.0.clear();
        self.wipe_from(0, self.0.capacity());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_limit() {
        let mut p = Password::new();
        p.push("hé");
        assert_eq!(p.chars(), 2);
        p.pop();
        assert_eq!(p.as_str(), "h");
        p.push(&"x".repeat(MAX * 2));
        assert_eq!(p.as_str(), "h", "too long for the buffer: ignored");
        let taken = p.take();
        assert!(p.is_empty());
        assert_eq!(taken.as_str(), "h");
    }

    #[test]
    fn pop_wipes_the_removed_bytes() {
        let mut p = Password::new();
        p.push("ab");
        p.pop();
        // SAFETY: reading back inside the allocation, which we just wrote.
        let b = unsafe { *p.0.as_ptr().add(1) };
        assert_eq!(b, 0);
    }
}

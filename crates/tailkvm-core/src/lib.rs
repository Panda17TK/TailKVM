//! Platform-independent domain logic shared across the TailKVM crates.
//!
//! Currently the seamless/multi-screen coordinate geometry (edge crossing,
//! return-edge detection, aspect-correct entry mapping). Kept free of any
//! `tailkvm_win32` / OS dependency so it stays the lowest layer everything else
//! can build on and remains directly unit-testable.

pub mod geometry;

pub mod account;
pub mod config;
pub mod filter;
pub mod label;
pub mod state;
pub mod triage;

// One way to expand `~`, shared across every scottidler Rust repo:
// https://github.com/scottidler/expand-tilde
//
// Re-exported here so call sites keep saying `cfg::expand_tilde` and the
// serde attributes keep pointing at `crate::cfg::...`. The crate carries the
// behavior decisions (bare `~` expands, `~otheruser` passes through, no
// fallback substitution when HOME is missing) and the tests that pin them.
pub use expand_tilde::{deserialize_tilde_pathbuf, deserialize_tilde_pathbuf_opt, expand_tilde};

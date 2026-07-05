//! Input synthesis over the XTEST extension.
//!
//! Split into `pointer` (motion, buttons, scroll, drag), `keyboard` (key
//! sequences, typing, hold), and `keymap` (CU key-spec parsing -> keysyms).
//! All v0.1 bodies are stubs with final signatures; a wave-2 agent fills them
//! in without touching main.rs/cli.rs.

pub mod keyboard;
pub mod keymap;
pub mod pointer;

#[cfg(test)]
mod xvfb_it;

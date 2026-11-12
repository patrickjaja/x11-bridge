//! Input synthesis over the XTEST extension.
//!
//! Split into `pointer` (motion, buttons, scroll, drag), `keyboard` (key
//! sequences, typing, hold), `keymap` (CU key-spec parsing -> keysyms),
//! `layout` (keysym -> key and level in the active XKB group) and `remap`
//! (spare keycodes for characters the layout lacks).
//! All v0.1 bodies are stubs with final signatures; a wave-2 agent fills them
//! in without touching main.rs/cli.rs.

pub mod keyboard;
pub mod keymap;
pub mod layout;
pub mod pointer;
pub mod remap;

#[cfg(test)]
mod xvfb_it;

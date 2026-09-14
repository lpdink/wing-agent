//! App-layer tests, split by subject.
//!
//! Rust visibility keeps them inside the `app` subtree: they reach `App`'s
//! private fields and the lanes' `pub(super)` entry points. `support` holds
//! only the fixtures shared by more than one file.

mod commands;
mod core;
mod frame;
mod goal_lane;
mod interaction;
mod modal;
mod projection;
mod scrollbar;
mod selection;
mod support;

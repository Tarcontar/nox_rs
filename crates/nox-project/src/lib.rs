//! Project and timeline data model: media items, sequences, tracks and clips.
//!
//! This crate is deliberately free of GTK, FFmpeg and Vulkan so it can be
//! tested headless and shared by the GUI, engine and CLI.

pub mod time;

pub use time::{Rational, Time, timecode};

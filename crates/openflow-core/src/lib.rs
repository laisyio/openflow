//! Everything OpenFlow does that does not draw a window.
//!
//! The Tauri shell and the AppKit binary are both thin hosts over this crate:
//! they own windows, trays and menus, and delegate capture, transcription,
//! insertion, storage and secrets to the modules below.

pub mod agreement;
pub mod audio;
pub mod db;
pub mod engine;
pub mod hotkey;
pub mod insert;
pub mod plugins;
pub mod postpass;
pub mod runner;
pub mod secrets;
pub mod settings;
pub mod speech;
pub mod transcribe;

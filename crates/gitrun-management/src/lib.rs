//! Public API for GitRun's shared management backend.
//! The implementation lives in `backend`; the optional Tauri desktop adapter
//! registers its public commands from that module without owning the backend.
pub mod backend;
pub use backend::*;

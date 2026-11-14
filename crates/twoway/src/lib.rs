//! twoway: two-way sync between Roblox Studio and folders on disk.
//!
//! The library holds everything a host needs: the engine, the file layout, model files, `build`
//! and `check`, and [`service::Service`], which answers the plugin's and the review page's
//! requests. The Rojo fork (github.com/Velover/rojo-two-way-sync) mounts it next to Rojo's API.

pub mod check;
pub mod config;
pub mod engine;
pub mod modeldiff;
pub mod modelfile;
pub mod place_build;
pub mod service;
pub mod tree;

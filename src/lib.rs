pub mod config;
pub mod db;
pub mod export;
pub mod model;
pub mod session;
pub mod sql;
// GUI lives in the lib (gui-gated) so headless UI tests can drive the real
// view; the binary is a thin launcher over it.
#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod guitheme;

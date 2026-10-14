pub mod backend;
pub mod bridge;
pub mod cli;
pub mod config;
pub mod host_tool;
pub mod mcp;
pub mod mount;
pub mod template;
pub mod view;

pub const APP_NAME: &str = env!("CARGO_CRATE_NAME");
pub const APP_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), env!("VERSION_SUFFIX"));

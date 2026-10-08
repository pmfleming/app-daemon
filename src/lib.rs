pub mod api;
#[cfg(feature = "benchmarks")]
pub mod benchmarks;
pub mod catalog;
pub mod client;
pub mod daemon;
pub mod history;
pub mod hyprland;
pub mod launch;
mod metrics;
pub mod model;
mod ownership;
mod platform;
mod process;
pub mod protocol;
pub mod resources;
pub mod service;
pub mod settings;

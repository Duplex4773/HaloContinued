pub mod catalog;
pub mod configuration;
pub mod controls;
pub mod dpi;
mod dpi_logitech;
mod logitech_adapter;
pub mod logitech_controls;
mod mchose_adapter;
pub mod mchose_controls;
mod passive_jbl;
pub mod protocols;
pub mod provider;
pub mod razer_controls;
pub use provider::{HidProvider, providers};

mod sleep_catalog;
pub mod sleep_controls;

pub mod utils;
#[macro_use]
pub mod macros;
#[macro_use]
extern crate log;

mod app;
mod app_delay;
mod config;
mod foreground;
mod keyboard;
mod painter;
mod session;
mod startup;
mod trayicon;

pub use crate::app::start;
pub use crate::config::{load_config, Config};

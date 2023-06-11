#[macro_use]
extern crate log;
extern crate pretty_env_logger;

use std::env;
use std::error::Error;

use config::Config;
use lazy_static::lazy_static;

use app::*;

mod app;
mod command;

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env::set_var("RUST_LOG", "p2p");

    color_eyre::install()?;
    pretty_env_logger::init();

    start().await?;
    Ok(())
}

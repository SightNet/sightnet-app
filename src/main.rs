#[macro_use]
extern crate log;
extern crate pretty_env_logger;

use std::env;
use std::error::Error;
use app::*;

mod app;

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env::set_var("RUST_LOG", "p2p");

    color_eyre::install()?;
    pretty_env_logger::init();

    start().await?;
    Ok(())
}

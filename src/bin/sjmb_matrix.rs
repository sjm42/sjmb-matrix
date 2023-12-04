// bin/sjmb_slack.rs

use clap::Parser;
use std::env;

use sjmb_matrix::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut opts = OptsCommon::parse();
    opts.finish()?;
    opts.start_pgm(env!("CARGO_BIN_NAME"));

    let bot = Bot::new(&opts).await?;
    bot.run().await?;

    Ok(())
}

// EOF

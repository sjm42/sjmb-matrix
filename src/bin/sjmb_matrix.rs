// bin/sjmb_slack.rs

use std::env;
use structopt::StructOpt;

use sjmb_matrix::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut opts = OptsCommon::from_args();
    opts.finish()?;
    opts.start_pgm(env!("CARGO_BIN_NAME"));

    let bot = Bot::new(&opts).await?;
    bot.run().await?;

    Ok(())
}

// EOF

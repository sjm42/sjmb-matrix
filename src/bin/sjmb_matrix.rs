// bin/sjmb_matrix.rs

use sjmb_matrix::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut opts = OptsCommon::parse();
    opts.finish()?;
    opts.start_pgm(env!("CARGO_BIN_NAME"))?;

    let bot = Bot::new(&opts).await?;
    if opts.login {
        bot.login().await?;
    } else if let Some(device) = opts.verify.as_deref() {
        bot.verify(device).await?;
    } else {
        bot.run().await?;
    }

    Ok(())
}

// EOF

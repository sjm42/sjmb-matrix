// config.rs

use std::env;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::*;

#[derive(Debug, Clone, Parser)]
pub struct OptsCommon {
    #[arg(short, long)]
    pub verbose: bool,
    #[arg(short, long)]
    pub debug: bool,
    #[arg(short, long)]
    pub trace: bool,

    #[arg(
        short,
        long,
        default_value = "$HOME/sjmb_matrix/config/sjmb_matrix.json"
    )]
    pub bot_config: String,
}

impl OptsCommon {
    pub fn finish(&mut self) -> anyhow::Result<()> {
        self.bot_config = shellexpand::full(&self.bot_config)?.into_owned();
        Ok(())
    }

    pub fn get_loglevel(&self) -> Level {
        if self.trace {
            Level::TRACE
        } else if self.debug {
            Level::DEBUG
        } else if self.verbose {
            Level::INFO
        } else {
            Level::ERROR
        }
    }

    fn get_log_filter(&self) -> String {
        let app_level = self.get_loglevel().as_str().to_ascii_lowercase();
        let dependency_level = if self.verbose || self.debug || self.trace {
            "warn"
        } else {
            "error"
        };

        format!("{dependency_level},sjmb_matrix={app_level}")
    }

    pub fn start_pgm(&self, name: &str) {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(self.get_log_filter()));

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();

        info!("Starting up {name} v{}...", env!("CARGO_PKG_VERSION"));
        debug!("Git branch: {}", env!("GIT_BRANCH"));
        debug!("Git commit: {}", env!("GIT_COMMIT"));
        debug!("Source timestamp: {}", env!("SOURCE_TIMESTAMP"));
        debug!("Compiler version: {}", env!("RUSTC_VERSION"));
    }
}

// EOF

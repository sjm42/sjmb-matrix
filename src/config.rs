// config.rs

use crate::*;

#[derive(Debug, Clone, Parser)]
pub struct OptsCommon {
    #[arg(short, long)]
    pub verbose: bool,
    #[arg(short, long)]
    pub debug: bool,
    #[arg(short, long)]
    pub trace: bool,

    /// Log in once, or explicitly reauthenticate the saved device.
    #[arg(long, conflicts_with = "verify")]
    pub login: bool,

    /// Verify with this desktop device ID on the same account (service must be stopped).
    #[arg(long, value_name = "DEVICE_ID")]
    pub verify: Option<String>,

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

        let mut filter = format!("{dependency_level},sjmb_matrix={app_level}");
        if !self.debug && !self.trace {
            // SDK 0.19 warns on every sync when backups are not configured.
            filter.push_str(",matrix_sdk_crypto::backups=error");
        }
        filter
    }

    pub fn start_pgm(&self, name: &str) -> anyhow::Result<()> {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(self.get_log_filter()));

        tracing_fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init()
            .map_err(|error| anyhow!("Could not initialize tracing: {error}"))?;

        info!("Starting up {name} v{}...", env!("CARGO_PKG_VERSION"));
        debug!("Git branch: {}", env!("GIT_BRANCH"));
        debug!("Git commit: {}", env!("GIT_COMMIT"));
        debug!("Source timestamp: {}", env!("SOURCE_TIMESTAMP"));
        debug!("Compiler version: {}", env!("RUSTC_VERSION"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_errors_and_other_modules_warnings_remain_visible() {
        for (flag, custom_filter, show_backup_warnings) in [
            ("-v", false, false),
            ("--debug", false, true),
            ("--trace", false, true),
            ("-v", true, true),
        ] {
            let opts = OptsCommon::parse_from(["sjmb_matrix", flag]);
            let custom_filter = custom_filter.then(|| EnvFilter::new("warn,sjmb_matrix=info"));
            let filter = custom_filter.unwrap_or_else(|| EnvFilter::new(opts.get_log_filter()));
            let file = tempfile::NamedTempFile::new().unwrap();
            let subscriber = tracing_fmt()
                .with_env_filter(filter)
                .with_target(false)
                .with_ansi(false)
                .without_time()
                .with_writer(file.reopen().unwrap())
                .finish();
            with_default(subscriber, || {
                warn!(target: "matrix_sdk_crypto::backups", "backup warning");
                error!(target: "matrix_sdk_crypto::backups", "backup error");
                warn!(target: "matrix_sdk_crypto::olm", "encryption warning");
                warn!(target: "sjmb_matrix", "app warning");
                info!(target: "sjmb_matrix", "app ready");
            });
            let output = fs::read_to_string(file.path()).unwrap();
            assert_eq!(
                output.contains("backup warning"),
                show_backup_warnings,
                "{output}"
            );
            assert!(output.contains("backup error"));
            assert!(output.contains("encryption warning"));
            assert!(output.contains("app warning"));
            assert!(output.contains("app ready"));
        }
    }
}

// EOF

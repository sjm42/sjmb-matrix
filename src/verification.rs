use std::io::{IsTerminal, Write};

use anyhow::{Context, bail, ensure};
use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    encryption::verification::{
        SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
        format_emojis,
    },
    ruma::{DeviceId, events::key::verification::VerificationMethod},
};
use tokio::time::{Duration, timeout};

use crate::{
    harvest::log_text, matrixbot::sync_client, runtime::shutdown_signal, session::SessionStore,
};

pub(crate) async fn verify(
    client: &Client,
    state: &SessionStore,
    device_id: &str,
) -> anyhow::Result<()> {
    ensure!(
        std::io::stdin().is_terminal(),
        "--verify requires an interactive terminal"
    );
    let user = client
        .user_id()
        .context("Matrix client is not authenticated")?;
    let peer: &DeviceId = device_id.into();
    ensure!(
        Some(peer) != client.device_id(),
        "Choose your desktop device, not the bot's own device ID"
    );
    let mut request: Option<VerificationRequest> = None;
    let flow = async {
        // Fetch current own-account keys/devices without bootstrapping or
        // resetting the existing cross-signing identity.
        client.encryption().request_user_identity(user).await?;
        let device = client
            .encryption()
            .get_device(user, peer)
            .await?
            .context("Desktop device not found on this account; check its device ID")?;
        println!(
            "Verifying {} / {} ({})",
            user,
            peer,
            log_text(device.display_name().unwrap_or("unnamed desktop"), 256)
        );
        let started = device
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await?;
        request = Some(started.clone());
        println!("Accept the verification request in your desktop client.");
        let sas = wait_for_sas(&started).await?;
        ensure!(
            sas.is_self_verification() && sas.other_device().device_id() == peer,
            "Verification peer changed unexpectedly"
        );
        compare_emojis(&sas).await
    };
    let result = tokio::select! {
        result = timeout(Duration::from_secs(180), flow) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("Verification exceeded three minutes"))),
        result = sync_client(client) => result.and_then(|()| Err(anyhow::anyhow!("Matrix sync stopped during verification"))),
        result = state.persistence_failed() => result,
        result = shutdown_signal() => result.and_then(|()| Err(anyhow::anyhow!("Verification interrupted"))),
    };
    if result.is_err()
        && let Some(request) = request
    {
        let _ = timeout(Duration::from_secs(3), request.cancel()).await;
    }
    result
}

async fn wait_for_sas(request: &VerificationRequest) -> anyhow::Result<SasVerification> {
    let mut changes = request.changes();
    while let Some(state) = changes.next().await {
        match state {
            VerificationRequestState::Ready { .. } => {
                if let Some(sas) = request.start_sas().await? {
                    return Ok(sas);
                }
            }
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
            } => return Ok(sas),
            VerificationRequestState::Cancelled(_) => bail!("Desktop cancelled verification"),
            VerificationRequestState::Done => {
                bail!("Verification ended without emoji confirmation")
            }
            _ => {}
        }
    }
    bail!("Verification request stopped")
}

async fn compare_emojis(sas: &SasVerification) -> anyhow::Result<()> {
    let mut changes = sas.changes();
    // Necessary if the desktop initiated the SAS start while we were waiting.
    sas.accept().await?;
    let mut confirmed = false;
    while let Some(state) = changes.next().await {
        match state {
            SasState::KeysExchanged { emojis, .. } if !confirmed => {
                let emojis = emojis.context("Desktop did not negotiate emoji verification")?;
                println!(
                    "Compare these emojis with your desktop:\n{}",
                    format_emojis(emojis.emojis)
                );
                if !confirm_prompt().await? {
                    sas.cancel().await?;
                    bail!("Verification rejected; device was not trusted");
                }
                sas.confirm().await?;
                confirmed = true;
                println!("Confirmed locally; waiting for your desktop to finish.");
            }
            SasState::Done { .. } => {
                ensure!(
                    confirmed,
                    "Verification completed without local confirmation"
                );
                println!(
                    "Verification completed. The Matrix device and keys will be retained across restarts."
                );
                return Ok(());
            }
            SasState::Cancelled(_) => bail!("Verification cancelled"),
            _ => {}
        }
    }
    bail!("Verification stopped before completion")
}

async fn confirm_prompt() -> anyhow::Result<bool> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<bool> {
            print!("Do all seven emojis match? Type yes to confirm: ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            Ok(confirmation(&input))
        })();
        let _ = tx.send(result);
    });
    rx.await
        .context("Confirmation input stopped")?
        .context("Could not read confirmation")
}

fn confirmation(input: &str) -> bool {
    input.trim().eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trust_requires_explicit_yes() {
        assert!(confirmation("yes\n"));
        assert!(confirmation(" YES "));
        for input in ["", "y", "true", "ok", "no"] {
            assert!(!confirmation(input));
        }
    }
}

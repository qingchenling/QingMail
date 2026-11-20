//! Only foreground actions request a system prompt; background credential reads
//! continue to report Locked and never repeatedly interrupt the desktop.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

impl SystemVault {
    /// No secret is read or returned. The system owns the password dialog.
    /// Returns whether an unlock was needed; cancellation stops waiting promptly.
    pub fn request_unlock(&self, cancel: &AtomicBool) -> Result<bool, VaultError> {
        wait_for_unlock(
            async {
                let service = connect().await?;
                unlock_default(&service).await
            },
            cancel,
        )
    }
}

fn wait_for_unlock<T>(
    operation: impl std::future::Future<Output = Result<T, VaultError>>,
    cancel: &AtomicBool,
) -> Result<T, VaultError> {
    if cancel.load(Ordering::Acquire) {
        return Err(VaultError::Locked);
    }
    bounded_for(
        futures_lite::future::or(operation, async {
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(VaultError::Locked);
                }
                async_io::Timer::after(Duration::from_millis(25)).await;
            }
        }),
        Duration::from_secs(120),
    )
}

async fn unlock_default(service: &SecretService<'_>) -> Result<bool, VaultError> {
    let collection = service
        .get_default_collection()
        .await
        .map_err(|_| VaultError::Unavailable)?;
    if !collection.is_locked().await.map_err(unlock_error)? {
        return Ok(false);
    }
    // unlock() issues Secret Service.Unlock and executes its returned Prompt.
    collection.unlock().await.map_err(unlock_error)?;
    if collection.is_locked().await.map_err(unlock_error)? {
        return Err(VaultError::Locked);
    }
    Ok(true)
}

fn unlock_error(error: secret_service::Error) -> VaultError {
    match error {
        secret_service::Error::Locked | secret_service::Error::Prompt => VaultError::Locked,
        secret_service::Error::Unavailable => VaultError::Unavailable,
        _ => VaultError::OperationFailed,
    }
}

#[cfg(test)]
#[path = "unlock_tests.rs"]
mod tests;

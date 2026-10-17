//! Where an API key comes from: an environment variable, or the operating
//! system's keychain through `keyring-core` (macOS Keychain, Windows
//! Credential Manager, Linux Secret Service).
//!
//! Every keychain call runs on one dedicated plain thread. The Linux store
//! drives its own runtime with `block_on`, which panics when the caller is
//! already inside a tokio worker and then poisons the store for the whole
//! process.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;

use keyring_core::{CredentialStore, Entry, Error};

#[derive(Debug, Clone, thiserror::Error)]
pub enum SecretError {
    #[error("the keychain is unavailable: {0}")]
    Unavailable(String),
    #[error("the keychain failed for {service}/{user}: {message}")]
    Keychain {
        service: String,
        user: String,
        message: String,
    },
}

pub trait SecretSource: Send + Sync {
    fn env(&self, name: &str) -> Option<String>;

    /// # Errors
    /// [`SecretError`] when the keychain cannot be opened or refuses the
    /// read. A credential that simply is not stored is `Ok(None)`.
    fn keychain(&self, service: &str, user: &str) -> Result<Option<String>, SecretError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSecrets;

impl SecretSource for SystemSecrets {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn keychain(&self, service: &str, user: &str) -> Result<Option<String>, SecretError> {
        let (service, user) = (service.to_string(), user.to_string());
        on_keychain_thread(move || {
            match Entry::new(&service, &user).and_then(|entry| entry.get_password()) {
                Ok(secret) => Ok(Some(secret)),
                Err(Error::NoEntry) => Ok(None),
                Err(error) => Err(keychain_error(&service, &user, &error)),
            }
        })
    }
}

/// # Errors
/// [`SecretError`] when the keychain cannot be opened or refuses the write.
pub fn store_in_keychain(service: &str, user: &str, secret: &str) -> Result<(), SecretError> {
    let (service, user, secret) = (service.to_string(), user.to_string(), secret.to_string());
    on_keychain_thread(move || {
        Entry::new(&service, &user)
            .and_then(|entry| entry.set_password(&secret))
            .map_err(|error| keychain_error(&service, &user, &error))
    })
}

/// # Errors
/// [`SecretError`] when the keychain cannot be opened or refuses the delete.
/// Returns whether a credential was actually removed.
pub fn delete_from_keychain(service: &str, user: &str) -> Result<bool, SecretError> {
    let (service, user) = (service.to_string(), user.to_string());
    on_keychain_thread(move || {
        match Entry::new(&service, &user).and_then(|entry| entry.delete_credential()) {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            Err(error) => Err(keychain_error(&service, &user, &error)),
        }
    })
}

fn keychain_error(service: &str, user: &str, error: &Error) -> SecretError {
    SecretError::Keychain {
        service: service.to_string(),
        user: user.to_string(),
        message: error.to_string(),
    }
}

type Job = Box<dyn FnOnce() + Send>;
type Factory = Box<dyn Fn() -> Result<Arc<CredentialStore>, SecretError> + Send + Sync>;

static KEYCHAIN_THREAD: OnceLock<Result<Sender<Job>, String>> = OnceLock::new();
static CUSTOM_STORE: OnceLock<Factory> = OnceLock::new();
static STORE_INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

/// Replaces the platform keychain with another credential store, opened on
/// first use. Answers false when one is already registered.
pub fn install_store_on_first_use(
    factory: impl Fn() -> Result<Arc<CredentialStore>, SecretError> + Send + Sync + 'static,
) -> bool {
    CUSTOM_STORE.set(Box::new(factory)).is_ok()
}

fn ensure_store() -> Result<(), SecretError> {
    STORE_INSTALLED
        .get_or_init(|| {
            let store = match CUSTOM_STORE.get() {
                Some(factory) => factory(),
                None => platform_store(),
            };
            store
                .map(keyring_core::set_default_store)
                .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(SecretError::Unavailable)
}

#[cfg(target_os = "macos")]
fn platform_store() -> Result<Arc<CredentialStore>, SecretError> {
    let store: Arc<CredentialStore> =
        apple_native_keyring_store::keychain::Store::new().map_err(|error| {
            SecretError::Unavailable(format!(
                "the macOS login keychain could not be opened: {error}"
            ))
        })?;
    Ok(store)
}

#[cfg(target_os = "linux")]
fn platform_store() -> Result<Arc<CredentialStore>, SecretError> {
    let store: Arc<CredentialStore> =
        zbus_secret_service_keyring_store::Store::new().map_err(|error| {
            SecretError::Unavailable(format!(
                "the Secret Service could not be reached ({error}); it needs a running keyring such as gnome-keyring and DBUS_SESSION_BUS_ADDRESS in the environment"
            ))
        })?;
    Ok(store)
}

#[cfg(target_os = "windows")]
fn platform_store() -> Result<Arc<CredentialStore>, SecretError> {
    let store: Arc<CredentialStore> =
        windows_native_keyring_store::Store::new().map_err(|error| {
            SecretError::Unavailable(format!(
                "the Windows Credential Manager could not be opened: {error}"
            ))
        })?;
    Ok(store)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_store() -> Result<Arc<CredentialStore>, SecretError> {
    Err(SecretError::Unavailable(
        "no keychain store ships for this platform".to_string(),
    ))
}

fn on_keychain_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, SecretError> + Send + 'static,
) -> Result<T, SecretError> {
    let (reply, answer) = mpsc::channel();
    let job: Job = Box::new(move || {
        if reply.send(ensure_store().and_then(|()| work())).is_err() {
            tracing::warn!("a keychain answer had no caller left to take it");
        }
    });
    keychain_thread()?.send(job).map_err(|error| {
        SecretError::Unavailable(format!("the keychain thread stopped: {error}"))
    })?;
    answer.recv().map_err(|error| {
        SecretError::Unavailable(format!(
            "a keychain call panicked before it answered: {error}"
        ))
    })?
}

fn keychain_thread() -> Result<&'static Sender<Job>, SecretError> {
    KEYCHAIN_THREAD
        .get_or_init(|| {
            let (sender, jobs) = mpsc::channel::<Job>();
            thread::Builder::new()
                .name("keychain".into())
                .spawn(move || {
                    for job in jobs {
                        if catch_unwind(AssertUnwindSafe(job)).is_err() {
                            tracing::warn!("a keychain call panicked and was reported");
                        }
                    }
                })
                .map(|_| sender)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|message| {
            SecretError::Unavailable(format!("the keychain thread could not start: {message}"))
        })
}

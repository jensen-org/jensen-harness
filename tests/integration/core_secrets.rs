use std::sync::Arc;

use jensen_harness_core::secrets::{
    SecretSource, SystemSecrets, delete_from_keychain, install_store_on_first_use,
    store_in_keychain,
};

#[test]
fn a_stored_key_reads_back_a_missing_one_is_none_and_a_deleted_one_is_gone() {
    let installed = install_store_on_first_use(|| {
        let store = keyring_core::mock::Store::new().map_err(|error| {
            jensen_harness_core::secrets::SecretError::Unavailable(error.to_string())
        })?;
        let store: Arc<keyring_core::CredentialStore> = store;
        Ok(store)
    });
    assert!(installed);

    assert_eq!(SystemSecrets.keychain("svc", "nobody").unwrap(), None);
    store_in_keychain("svc", "alice", "sk-secret").unwrap();
    assert_eq!(
        SystemSecrets.keychain("svc", "alice").unwrap().as_deref(),
        Some("sk-secret")
    );
    assert!(delete_from_keychain("svc", "alice").unwrap());
    assert_eq!(SystemSecrets.keychain("svc", "alice").unwrap(), None);
    assert!(!delete_from_keychain("svc", "alice").unwrap());
}

#[test]
fn the_environment_source_reads_the_process_environment() {
    assert!(
        SystemSecrets
            .env("JENSEN_HARNESS_TEST_DEFINITELY_UNSET_55E1")
            .is_none()
    );
}

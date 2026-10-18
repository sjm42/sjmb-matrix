//! One local owner for a Matrix device, its tokens and its SDK store.

use crate::*;

const SESSION_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct SavedSession {
    version: u32,
    homeserver: String,
    configured_user: String,
    device_id: String,
    encryption: bool,
    store_passphrase: String,
    session: Option<MatrixSession>,
}

pub(crate) struct SessionStore {
    directory: PathBuf,
    saved: Mutex<SavedSession>,
    failed: watch::Sender<bool>,
    _lock: File,
}

impl SessionStore {
    pub(crate) fn open(
        directory: &str,
        server: &str,
        user: &str,
        login: bool,
    ) -> anyhow::Result<Arc<Self>> {
        info!(
            "Opening Matrix state directory {}",
            crate::harvest::log_text(directory, 1024)
        );
        let directory = PathBuf::from(directory);
        private_directory(&directory)?;
        let lock_path = directory.join("session.lock");
        check_private_file(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let lock = options
            .open(lock_path)
            .context("Could not open Matrix session lock")?;
        lock.try_lock().context(
            "Matrix state is already in use; stop the service before login or verification",
        )?;
        let path = directory.join("session.json");
        check_private_file(&path)?;
        let homeserver = url::Url::parse(server)?.to_string();
        let saved = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(SESSION_BYTES + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() as u64 <= SESSION_BYTES,
                    "Saved Matrix session exceeds 64 KiB"
                );
                let saved: SavedSession = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow!("Invalid saved Matrix session; restore the state directory from backup")
                })?;
                ensure!(
                    saved.version == 1,
                    "Unsupported saved Matrix session version"
                );
                ensure!(
                    saved.homeserver == homeserver && saved.configured_user == user,
                    "Saved Matrix session belongs to another homeserver or configured account"
                );
                ensure!(
                    saved.encryption == cfg!(feature = "e2e-encryption"),
                    "Saved Matrix device encryption setting differs from this build"
                );
                ensure!(
                    !saved.store_passphrase.is_empty() && !saved.device_id.is_empty(),
                    "Incomplete Matrix device state"
                );
                if let Some(session) = &saved.session {
                    ensure!(
                        session.meta.device_id.as_str() == saved.device_id
                            && account_matches(user, session.meta.user_id.as_str()),
                        "Saved Matrix account/device identity mismatch"
                    );
                    let mut files = vec!["matrix-sdk-state.sqlite3"];
                    if saved.encryption {
                        files.push("matrix-sdk-crypto.sqlite3");
                    }
                    for name in files {
                        let metadata = fs::symlink_metadata(directory.join("store").join(name))
                            .context("Matrix store files are missing; restore the complete state directory")?;
                        ensure!(
                            metadata.is_file() && metadata.len() > 0,
                            "Matrix store file is invalid; restore the complete state directory"
                        );
                    }
                }
                ensure!(
                    directory.join("store").is_dir(),
                    "Matrix crypto/state store is missing; restore the complete state directory"
                );
                saved
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure!(login, "No saved Matrix session; run with --login once");
                ensure!(
                    !directory.join("store").exists(),
                    "Matrix store exists without session metadata; restore the complete state directory or choose a new matrix_state_dir"
                );
                private_directory(&directory.join("store"))?;
                SavedSession {
                    version: 1,
                    homeserver,
                    configured_user: user.into(),
                    device_id: format!("SJMB{}", Alphanumeric.sample_string(&mut rand::rng(), 20)),
                    encryption: cfg!(feature = "e2e-encryption"),
                    store_passphrase: Alphanumeric.sample_string(&mut rand::rng(), 48),
                    session: None,
                }
            }
            Err(error) => return Err(error).context("Could not read saved Matrix session"),
        };
        let (failed, _) = watch::channel(false);
        let store = Arc::new(Self {
            directory,
            saved: Mutex::new(saved),
            failed,
            _lock: lock,
        });
        // Persist the chosen device and passphrase before login. A failed or
        // interrupted login can explicitly retry with the same device/store.
        if login {
            store.write(&store.saved.lock().unwrap())?;
        }
        Ok(store)
    }

    pub(crate) async fn connect(
        self: &Arc<Self>,
        password: Option<&str>,
    ) -> anyhow::Result<Client> {
        let (homeserver, passphrase, device_id, configured_user, previous) = {
            let saved = self.saved.lock().unwrap();
            (
                saved.homeserver.clone(),
                saved.store_passphrase.clone(),
                saved.device_id.clone(),
                saved.configured_user.clone(),
                saved.session.clone(),
            )
        };
        ensure!(
            password.is_some() || previous.is_some(),
            "Login is incomplete; run with --login again"
        );
        let builder = Client::builder()
            .homeserver_url(&homeserver)
            // MAS uses the model/OS segments to label legacy-login sessions.
            .user_agent(format!(
                "sjmb-matrix/{} (sjmb-matrix; {}; {})",
                env!("CARGO_PKG_VERSION"),
                env::consts::OS,
                env::consts::ARCH,
            ))
            .respect_login_well_known(false)
            .sqlite_store(self.directory.join("store"), Some(&passphrase))
            // SDK 0.19 keeps the event cache inactive unless subscribed to.
            // This bot uses live event handlers and never subscribes to it.
            .with_enable_automatic_back_pagination(false)
            .handle_refresh_tokens()
            .request_config(RequestConfig::new().timeout(Duration::from_secs(15)));
        #[cfg(feature = "e2e-encryption")]
        let builder = builder
            .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: false,
                auto_enable_backups: false,
                backup_download_strategy: matrix_sdk::encryption::BackupDownloadStrategy::Manual,
            })
            .with_enable_share_history_on_invite(false);
        info!("Opening encrypted Matrix SDK store");
        let opened_at = time::Instant::now();
        let client = builder
            .build()
            .await
            .context("Could not open Matrix SDK store")?;
        info!(
            elapsed_ms = opened_at.elapsed().as_millis(),
            "Matrix SDK store opened"
        );
        let reload = self.clone();
        let save = self.clone();
        client.set_session_callbacks(
            Box::new(move |_| reload.tokens().map_err(Into::into)),
            Box::new(move |client| {
                let result = save.save_client(&client);
                if result.is_err() {
                    save.failed.send_replace(true);
                }
                result.map_err(Into::into)
            }),
        )?;
        if let Some(password) = password {
            ensure!(!password.is_empty(), "Matrix password must not be empty");
            let login_user = previous
                .as_ref()
                .map_or(configured_user.as_str(), |session| {
                    session.meta.user_id.as_str()
                });
            info!(
                device = device_id,
                "Logging in to Matrix with the saved device identity"
            );
            client
                .matrix_auth()
                .login_username(login_user, password)
                .device_id(&device_id)
                .initial_device_display_name("sjmb-matrix")
                .request_refresh_token()
                .send()
                .await
                .context("Matrix login failed; retry explicitly with --login")?;
            self.save_client(&client)?;
        } else {
            info!(device = device_id, "Restoring saved Matrix session");
            client.matrix_auth().restore_session(previous.unwrap(), Default::default()).await
                .context("Could not restore Matrix device; reauthenticate explicitly with --login if needed")?;
            info!("Matrix session restored");
        }
        Ok(client)
    }

    fn tokens(&self) -> anyhow::Result<SessionTokens> {
        self.saved
            .lock()
            .unwrap()
            .session
            .as_ref()
            .map(|session| session.tokens.clone())
            .context("Matrix session has no saved credentials")
    }

    fn save_client(&self, client: &Client) -> anyhow::Result<()> {
        let session = client
            .matrix_auth()
            .session()
            .context("Matrix client is not authenticated")?;
        let mut saved = self.saved.lock().unwrap();
        ensure!(
            session.meta.device_id.as_str() == saved.device_id
                && account_matches(&saved.configured_user, session.meta.user_id.as_str()),
            "Matrix login returned an unexpected account/device"
        );
        if let Some(previous) = &saved.session {
            ensure!(
                session.meta == previous.meta,
                "Matrix session identity changed unexpectedly"
            );
        }
        saved.session = Some(session);
        self.write(&saved)
    }

    fn write(&self, saved: &SavedSession) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(saved)?;
        ensure!(
            bytes.len() as u64 <= SESSION_BYTES,
            "Matrix credentials exceed the saved-session size limit"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("session.json"))
            .map_err(|_| anyhow!("Could not replace saved Matrix session"))?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub(crate) async fn persistence_failed(&self) -> anyhow::Result<()> {
        let mut failed = self.failed.subscribe();
        loop {
            if *failed.borrow_and_update() {
                bail!(
                    "Could not persist refreshed Matrix credentials; fix state storage and reauthenticate with --login if needed"
                );
            }
            failed.changed().await?;
        }
    }
}

fn account_matches(configured: &str, actual: &str) -> bool {
    if configured.starts_with('@') {
        configured == actual
    } else {
        matrix_sdk::ruma::UserId::parse(actual).is_ok_and(|user| user.localpart() == configured)
    }
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        builder.mode(0o700);
    }
    builder.create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Matrix state directory must be a real directory"
    );
    #[cfg(unix)]
    {
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Matrix state directory must be private (chmod 700)"
        );
    }
    Ok(())
}

fn check_private_file(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Matrix session files must be regular files"
            );
            #[cfg(unix)]
            {
                ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "Matrix session files must be private (chmod 600)"
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Use an ordinary thread for terminal input: Tokio's blocking pool would keep
/// runtime shutdown waiting for an unfinished terminal read after SIGTERM.
pub(crate) async fn password_prompt() -> anyhow::Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    thread::spawn(move || {
        let _ = tx.send(rpassword::prompt_password("Matrix password: "));
    });
    rx.await
        .context("Password input stopped")?
        .context("Could not read Matrix password")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_private_locked_bound_and_reuses_the_chosen_device() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let path = root.as_path().join("matrix");
        let path = path.to_str().unwrap();
        assert!(SessionStore::open(path, "https://example.org", "@me:example.org", false).is_err());
        let store =
            SessionStore::open(path, "https://example.org", "@me:example.org", true).unwrap();
        let id = store.saved.lock().unwrap().device_id.clone();
        assert!(SessionStore::open(path, "https://example.org", "@me:example.org", true).is_err());
        drop(store);
        assert!(
            SessionStore::open(path, "https://elsewhere.org", "@me:example.org", false).is_err()
        );
        assert!(
            SessionStore::open(path, "https://example.org", "@other:example.org", false).is_err()
        );
        let store =
            SessionStore::open(path, "https://example.org/", "@me:example.org", true).unwrap();
        assert_eq!(store.saved.lock().unwrap().device_id, id);
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.directory.join("session.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn missing_crypto_store_and_corrupt_credentials_fail_without_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let path = root.as_path().to_str().unwrap();
        let store = SessionStore::open(path, "https://example.org", "me", true).unwrap();
        drop(store);
        fs::write(root.as_path().join("session.json"), b"not json").unwrap();
        assert!(SessionStore::open(path, "https://example.org", "me", true).is_err());
        assert_eq!(
            fs::read(root.as_path().join("session.json")).unwrap(),
            b"not json"
        );
    }

    #[tokio::test]
    async fn sdk_restore_keeps_device_keys_and_refreshed_credentials() {
        let server = homeserver().await;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let directory = root.as_path().to_str().unwrap();
        let store = SessionStore::open(directory, &server.uri(), USER, true).unwrap();
        let client = store.connect(Some("fake-password")).await.unwrap();
        #[cfg(feature = "e2e-encryption")]
        client
            .encryption()
            .wait_for_e2ee_initialization_tasks()
            .await;
        let device_id = client.device_id().unwrap().to_owned();
        #[cfg(feature = "e2e-encryption")]
        let key = client
            .encryption()
            .get_own_device()
            .await
            .unwrap()
            .unwrap()
            .curve25519_key()
            .unwrap();
        client.matrix_auth().refresh_access_token().await.unwrap();
        let saved = read_saved(root.as_path());
        assert_eq!(
            saved.session.unwrap().tokens.access_token,
            "refreshed-token"
        );
        drop(client);
        drop(store);
        // SDK initialization tasks may briefly retain a client clone.
        let store = reopen(directory, &server.uri()).await;
        let restored = store.connect(None).await.unwrap();
        assert_eq!(restored.device_id(), Some(device_id.as_ref()));
        assert_eq!(restored.access_token().as_deref(), Some("refreshed-token"));
        #[cfg(feature = "e2e-encryption")]
        {
            restored
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await;
            assert_eq!(
                restored
                    .encryption()
                    .get_own_device()
                    .await
                    .unwrap()
                    .unwrap()
                    .curve25519_key()
                    .unwrap(),
                key
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path().ends_with("/login"))
                .count(),
            1
        );
        assert!(!requests.iter().any(|request| {
            request.url.path().contains("/keys/device_signing/upload")
                || request.url.path().contains("/messages")
        }));
    }

    #[tokio::test]
    async fn token_save_failure_requests_service_stop_and_missing_store_is_rejected() {
        let server = homeserver().await;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let directory = root.as_path().to_str().unwrap();
        let store = SessionStore::open(directory, &server.uri(), USER, true).unwrap();
        let client = store.connect(Some("fake-password")).await.unwrap();
        #[cfg(feature = "e2e-encryption")]
        client
            .encryption()
            .wait_for_e2ee_initialization_tasks()
            .await;
        fs::rename(
            root.as_path().join("session.json"),
            root.as_path().join("session.backup"),
        )
        .unwrap();
        fs::create_dir(root.as_path().join("session.json")).unwrap();
        client.matrix_auth().refresh_access_token().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), store.persistence_failed())
                .await
                .unwrap()
                .is_err()
        );
        drop(client);
        drop(store);
        fs::remove_dir(root.as_path().join("session.json")).unwrap();
        fs::rename(
            root.as_path().join("session.backup"),
            root.as_path().join("session.json"),
        )
        .unwrap();
        fs::rename(
            root.as_path().join("store"),
            root.as_path().join("missing-store"),
        )
        .unwrap();
        let error = match SessionStore::open(directory, &server.uri(), USER, true) {
            Ok(_) => panic!("missing store accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("store"));
    }

    fn read_saved(directory: &Path) -> SavedSession {
        serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap()
    }

    async fn reopen(directory: &str, server: &str) -> Arc<SessionStore> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(store) =
                    SessionStore::open(directory, server, crate::test_support::USER, false)
                {
                    return store;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SDK did not release the saved session lock")
    }
}

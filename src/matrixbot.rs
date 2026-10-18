use crate::*;

const CONFIG_BYTES: u64 = 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const RECENT_EVENTS: usize = 8192;

#[derive(Deserialize)]
pub struct Bot {
    url_regex: String,
    url_log_db: String,
    matrix_server: String,
    matrix_user_id: String,
    #[serde(default)]
    matrix_password: Option<String>,
    #[serde(default = "default_state_dir")]
    matrix_state_dir: String,
    #[serde(skip)]
    url_re: Option<Regex>,
}

impl fmt::Debug for Bot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bot")
            .field("url_log_db", &"[REDACTED]")
            .field("matrix_password", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

pub(crate) struct QueuedMessage {
    event_id: OwnedEventId,
    sender: OwnedUserId,
    timestamp: i64,
    text: String,
    room: Room,
}

enum Incoming {
    Plain(QueuedMessage),
    #[cfg(feature = "e2e-encryption")]
    Encrypted(crate::encrypted::EncryptedMessage),
}

fn default_state_dir() -> String {
    "$HOME/.local/state/sjmb-matrix".into()
}

/// Remember successful event IDs, never URL values: repeated posts are valid.
#[derive(Default)]
struct RecentEvents {
    ids: HashSet<OwnedEventId>,
    order: VecDeque<OwnedEventId>,
}

impl RecentEvents {
    fn remember(&mut self, id: OwnedEventId) {
        if !self.ids.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
        if self.order.len() > RECENT_EVENTS
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
    }
}

impl Bot {
    /// Load and validate locally before performing any network operations.
    pub async fn new(opts: &OptsCommon) -> anyhow::Result<Self> {
        info!("Reading config file {}", log_text(&opts.bot_config, 1024));
        let mut bytes = Vec::new();
        File::open(&opts.bot_config)
            .context("Could not open bot configuration")?
            .take(CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Could not read bot configuration")?;
        Self::from_config(&bytes)
    }

    fn from_config(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() as u64 <= CONFIG_BYTES,
            "Bot configuration exceeds 1 MiB"
        );
        // Serde's type errors can echo string values, including credentials.
        let mut bot: Self = serde_json::from_slice(bytes).map_err(|error| {
            anyhow!(
                "Invalid bot configuration JSON at line {}, column {}",
                error.line(),
                error.column()
            )
        })?;
        ensure!(
            !bot.matrix_user_id.trim().is_empty(),
            "matrix_user_id must not be empty"
        );
        ensure!(
            bot.matrix_password
                .as_ref()
                .is_none_or(|password| !password.is_empty()),
            "matrix_password must not be empty when supplied"
        );
        bot.matrix_state_dir = shellexpand::full(&bot.matrix_state_dir)?.into_owned();
        ensure!(
            !bot.matrix_state_dir.trim().is_empty(),
            "matrix_state_dir must not be empty"
        );
        let server = Url::parse(&bot.matrix_server)
            .map_err(|_| anyhow!("matrix_server must be an absolute HTTP/HTTPS URL"))?;
        ensure!(
            matches!(server.scheme(), "http" | "https")
                && server.host_str().is_some()
                && server.username().is_empty()
                && server.password().is_none()
                && server.query().is_none()
                && server.fragment().is_none(),
            "matrix_server must be an HTTP/HTTPS URL without credentials, query or fragment"
        );
        bot.url_log_db = shellexpand::full(&bot.url_log_db)?.into_owned();
        bot.url_log_db
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|_| anyhow!("Invalid url_log_db connection options"))?;
        let regex = Regex::new(&bot.url_regex).context("Invalid url_regex")?;
        ensure!(
            regex.captures_len() >= 2,
            "url_regex requires capture group 1"
        );
        bot.url_re = Some(regex);
        info!("Configuration validated");
        Ok(bot)
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.run_until(shutdown_signal()).await
    }

    pub async fn login(self) -> anyhow::Result<()> {
        let login = async {
            let state = SessionStore::open(
                &self.matrix_state_dir,
                &self.matrix_server,
                &self.matrix_user_id,
                true,
            )?;
            let password = match &self.matrix_password {
                Some(password) => password.clone(),
                None => password_prompt().await?,
            };
            let client = timeout(STARTUP_TIMEOUT, state.connect(Some(&password)))
                .await
                .context("Matrix login exceeded 30 seconds")??;
            #[cfg(feature = "e2e-encryption")]
            {
                // Login has already saved the credentials. Keep the runtime and
                // store alive while the SDK finishes its background setup.
                info!("Finishing Matrix encryption initialization");
                if timeout(
                    STARTUP_TIMEOUT,
                    client.encryption().wait_for_e2ee_initialization_tasks(),
                )
                .await
                .is_err()
                {
                    warn!(
                        "Matrix session was saved, but encryption initialization exceeded 30 seconds; start the bot normally to resume"
                    );
                }
            }
            println!(
                "Saved Matrix session for {} / {} (sjmb-matrix).",
                client.user_id().unwrap(),
                client.device_id().unwrap()
            );
            println!("You can remove matrix_password from the config.");
            #[cfg(feature = "e2e-encryption")]
            println!(
                "If this device is not already verified, run --verify DESKTOP_DEVICE_ID. Otherwise start the bot normally."
            );
            Ok(())
        };
        tokio::select! { biased; result = shutdown_signal() => result, result = login => result }
    }

    pub async fn verify(self, device: &str) -> anyhow::Result<()> {
        #[cfg(feature = "e2e-encryption")]
        {
            let restore = async {
                let state = SessionStore::open(
                    &self.matrix_state_dir,
                    &self.matrix_server,
                    &self.matrix_user_id,
                    false,
                )?;
                let client = timeout(STARTUP_TIMEOUT, state.connect(None))
                    .await
                    .context("Matrix restore exceeded 30 seconds")??;
                Ok::<_, anyhow::Error>((client, state))
            };
            let (client, state) = tokio::select! {
                biased;
                result = shutdown_signal() => { result?; return Ok(()); },
                result = restore => result?,
            };
            crate::verification::verify(&client, &state, device).await
        }
        #[cfg(not(feature = "e2e-encryption"))]
        {
            let _ = device;
            anyhow::bail!("--verify requires the e2e-encryption build feature")
        }
    }

    async fn run_until(
        self,
        shutdown: impl Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        let regex = self
            .url_re
            .as_ref()
            .context("Bot configuration has not been validated")?;
        let db = start_db(&self.url_log_db).await?;
        tokio::pin!(shutdown);
        let client = tokio::select! {
            biased;
            result = &mut shutdown => return result,
            result = timeout(STARTUP_TIMEOUT, async {
                let state = SessionStore::open(&self.matrix_state_dir, &self.matrix_server, &self.matrix_user_id, false)?;
                let client = state.connect(None).await?;
                // Establish a current baseline without harvesting startup backlog.
                // The SDK still processes room state and to-device key messages.
                info!("Starting initial Matrix sync");
                client.sync_once(SyncSettings::default().timeout(Duration::ZERO)).await.context("Initial Matrix sync failed; use --login if the saved session was revoked")?;
                info!(rooms = client.joined_rooms().len(), "Initial Matrix sync complete");
                Ok::<_, anyhow::Error>((client, state))
            }) => result.context("Matrix startup exceeded 30 seconds")??,
        };
        let (client, state) = client;
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel(MESSAGE_QUEUE_BOUND);
        let admission_counters = counters.clone();
        let plain_tx = tx.clone();
        let handler =
            client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let tx = plain_tx.clone();
                let counters = admission_counters.clone();
                async move {
                    if let Some(message) = prepare_message(event, room, &counters) {
                        // Matrix sync can wait for capacity; don't discard on overflow.
                        let _ = tx.send(Incoming::Plain(message)).await;
                    }
                }
            });
        let _handler_guard = client.event_handler_drop_guard(handler);
        #[cfg(feature = "e2e-encryption")]
        let key_updates = Arc::new(tokio::sync::Notify::new());
        #[cfg(feature = "e2e-encryption")]
        let _encrypted_guard = {
            let counters = counters.clone();
            let handle = client.add_event_handler(
                move |event: OriginalSyncRoomEncryptedEvent, room: Room, raw: RawEvent| {
                    let tx = tx.clone();
                    let counters = counters.clone();
                    async move {
                        if room.state() != RoomState::Joined {
                            return;
                        }
                        if raw.0.get().len() > crate::encrypted::MAX_CIPHERTEXT_BYTES {
                            counters.encrypted_discarded.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        let message = crate::encrypted::EncryptedMessage::new(
                            Raw::from_json(raw.0),
                            room,
                            event.event_id,
                        );
                        let _ = tx.send(Incoming::Encrypted(message)).await;
                    }
                },
            );
            client.event_handler_drop_guard(handle)
        };
        #[cfg(feature = "e2e-encryption")]
        let mut keys = client
            .encryption()
            .room_keys_received_stream()
            .await
            .context("Matrix encryption was not initialized")?;
        let (drain, requested) = watch::channel(false);
        info!(
            e2ee = cfg!(feature = "e2e-encryption"),
            "Watching joined Matrix rooms for new messages"
        );
        supervise(
            async {
                let key_listener = async {
                    #[cfg(feature = "e2e-encryption")]
                    {
                        while keys.next().await.is_some() {
                            key_updates.notify_one();
                        }
                        Err::<(), _>(anyhow!("Matrix room-key stream stopped"))
                    }
                    #[cfg(not(feature = "e2e-encryption"))]
                    future::pending::<anyhow::Result<()>>().await
                };
                tokio::select! {
                    result = sync_client(&client) => result,
                    result = state.persistence_failed() => result,
                    result = key_listener => result,
                }
            },
            handle_messages(
                rx,
                regex,
                &db,
                &counters,
                requested,
                #[cfg(feature = "e2e-encryption")]
                key_updates.clone(),
            ),
            shutdown,
            drain,
            counters.clone(),
        )
        .await
    }
}

pub(crate) async fn sync_client(client: &Client) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        // Reuse the same client so retries retain the SDK's latest sync token.
        let error = match client.sync(SyncSettings::default()).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        let Some(delay) = sync_retry_delay(&error, backoff) else {
            return Err(error).context("Matrix sync failed permanently");
        };
        error!(
            ?delay,
            "Matrix sync interrupted; retrying with the current session"
        );
        sleep(delay).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn sync_retry_delay(error: &matrix_sdk::Error, backoff: Duration) -> Option<Duration> {
    if let Some(api) = error.as_client_api_error() {
        if let Some(ErrorKind::LimitExceeded(limit)) = api.error_kind() {
            let retry_after = match limit.retry_after.as_ref() {
                Some(RetryAfter::Delay(delay)) => Some(*delay),
                Some(RetryAfter::DateTime(date)) => {
                    date.duration_since(time::SystemTime::now()).ok()
                }
                _ => None,
            };
            return Some(backoff.max(retry_after.unwrap_or_default()));
        }
        let status = api.status_code.as_u16();
        return (status == 408 || status == 429 || api.status_code.is_server_error())
            .then_some(backoff);
    }
    match error {
        matrix_sdk::Error::Http(http) if matches!(http.as_ref(), HttpError::Reqwest(_)) => {
            Some(backoff)
        }
        _ => None,
    }
}

pub(crate) fn prepare_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    counters: &Counters,
) -> Option<QueuedMessage> {
    if room.state() != RoomState::Joined {
        return None;
    }
    let MessageType::Text(text) = event.content.msgtype else {
        return None;
    };
    if text.body.len() > MAX_MESSAGE_BYTES {
        counters.oversized.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(QueuedMessage {
        event_id: event.event_id,
        sender: event.sender,
        timestamp: i64::from(event.origin_server_ts.as_secs()),
        text: text.body,
        room,
    })
}

async fn handle_messages(
    mut rx: mpsc::Receiver<Incoming>,
    regex: &Regex,
    db: &DbCtx,
    counters: &Counters,
    mut drain: watch::Receiver<bool>,
    #[cfg(feature = "e2e-encryption")] keys: Arc<tokio::sync::Notify>,
) {
    let mut recent = RecentEvents::default();
    let mut draining = false;
    #[cfg(feature = "e2e-encryption")]
    let mut pending = crate::encrypted::PendingEvents::default();
    #[cfg(feature = "e2e-encryption")]
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    #[cfg(feature = "e2e-encryption")]
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        #[cfg(feature = "e2e-encryption")]
        let retry_enabled = !draining && pending.len() > 0;
        #[cfg(not(feature = "e2e-encryption"))]
        let retry_enabled = false;
        let retry_ready = async {
            #[cfg(feature = "e2e-encryption")]
            tokio::select! { _ = retry.tick() => {}, _ = keys.notified() => {} }
            #[cfg(not(feature = "e2e-encryption"))]
            future::pending::<()>().await
        };
        let message = tokio::select! {
            _ = drain.changed(), if !draining => {
                draining = true;
                rx.close();
                #[cfg(feature = "e2e-encryption")]
                mem::take(&mut pending).discard(counters);
                continue;
            },
            _ = retry_ready, if retry_enabled => {
                #[cfg(feature = "e2e-encryption")]
                for _ in 0..pending.len().min(16) {
                    if let Some(message) = pending.retry_one(counters).await {
                        process_message(message, &mut recent, regex, db, counters).await;
                    }
                }
                continue;
            },
            message = rx.recv() => message,
        };
        let Some(message) = message else { return };
        match message {
            Incoming::Plain(message) => {
                process_message(message, &mut recent, regex, db, counters).await
            }
            #[cfg(feature = "e2e-encryption")]
            Incoming::Encrypted(message) if !draining => pending.push(message, counters),
            #[cfg(feature = "e2e-encryption")]
            Incoming::Encrypted(_) => {
                counters.encrypted_discarded.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn process_message(
    message: QueuedMessage,
    recent: &mut RecentEvents,
    regex: &Regex,
    db: &DbCtx,
    counters: &Counters,
) {
    if recent.ids.contains(&message.event_id) {
        counters.duplicates.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let id = message.event_id.clone();
    if handle_msg(message, regex, db, counters).await {
        recent.remember(id);
    }
}

async fn handle_msg(
    message: QueuedMessage,
    regex: &Regex,
    db: &DbCtx,
    counters: &Counters,
) -> bool {
    if message.room.state() != RoomState::Joined {
        return true;
    }
    let nick = match timeout(LOOKUP_TIMEOUT, message.room.get_member(&message.sender)).await {
        Ok(Ok(Some(member))) => member
            .display_name()
            .unwrap_or(message.sender.as_str())
            .to_owned(),
        _ => message.sender.to_string(),
    };
    let nick = attribution(&nick, message.sender.as_str());
    let room_name = message.room.name().unwrap_or_default();
    let room_name = attribution(&room_name, message.room.room_id().as_str());
    let text = message.text.trim();
    info!(
        "#[{}] <{}>: {}",
        log_text(&room_name, 1024),
        log_text(&nick, 256),
        log_text(text, 4096)
    );
    let mut success = true;
    for url in extract_urls(regex, text, counters) {
        info!(
            "*** on {} detected url: {}",
            log_text(&room_name, 1024),
            log_text(url, 2048)
        );
        match db_add_url(
            db,
            &UrlCtx {
                ts: message.timestamp,
                chan: format!("matrix-{room_name}"),
                nick: nick.clone(),
                url: url.to_owned(),
            },
        )
        .await
        {
            Ok(rows) => info!("Urllog: inserted {rows} row(s)"),
            Err(error) => {
                success = false;
                error!(
                    "URL insert failed: {}",
                    log_text(&format!("{error:#}"), 2048)
                );
            }
        }
    }
    success
}

fn attribution(name: &str, fallback: &str) -> String {
    let label = name.ws_convert();
    if label.is_empty() || label.chars().any(char::is_control) {
        fallback.to_owned()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> serde_json::Value {
        serde_json::json!({
            "url_regex": "<(https?://[^>]+)>",
            "url_log_db": "postgres://localhost:1/url?sslmode=disable",
            "matrix_server": "https://example.org",
            "matrix_user_id": "bot",
            "matrix_password": "secret-password"
        })
    }

    fn parse(value: &serde_json::Value) -> anyhow::Result<Bot> {
        Bot::from_config(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn validation_is_local_and_rejects_bad_configuration() {
        assert!(parse(&config()).is_ok());
        for (key, value) in [
            ("matrix_user_id", " "),
            ("matrix_password", ""),
            ("matrix_server", "file:///tmp/socket"),
            ("matrix_server", "https://user:secret@example.org"),
            ("url_log_db", "postgres:///url?sslmode=invalid"),
            ("url_regex", "https?://.*"),
            ("url_regex", "("),
        ] {
            let mut config = config();
            config[key] = value.into();
            assert!(parse(&config).is_err(), "{key}");
        }
        assert!(Bot::from_config(&vec![b' '; CONFIG_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn diagnostics_do_not_expose_config_secrets() {
        let bot = parse(&config()).unwrap();
        let debug = format!("{bot:?}");
        assert!(!debug.contains("secret-password"));
        assert!(!debug.contains("postgres://"));
        let error = Bot::from_config(br#"{"matrix_password": ["secret-password"]}"#).unwrap_err();
        assert!(!format!("{error:#}").contains("secret-password"));
    }

    #[test]
    fn attribution_keeps_normal_names_and_falls_back_for_unsafe_names() {
        assert_eq!(attribution(" Room\t name ", "!id:example.org"), "Room_name");
        for name in ["", " \n", "bad\0name", "\u{1b}[31m"] {
            assert_eq!(
                attribution(name, "@sender:example.org"),
                "@sender:example.org"
            );
        }
    }

    #[test]
    fn sync_retries_transient_errors_and_observes_server_retry_after() {
        let error = |status, body| {
            let api = matrix_sdk::ruma::api::error::Error::new(status, body);
            matrix_sdk::Error::from(HttpError::from(FromHttpResponseError::Server(
                UiaaResponse::MatrixError(api),
            )))
        };
        let backoff = Duration::from_secs(2);
        for (status, retry) in [
            (503u16, true),
            (502, true),
            (408, true),
            (429, true),
            (401, false),
            (403, false),
            (400, false),
        ] {
            let error = error(
                status.try_into().unwrap(),
                ErrorBody::Json(serde_json::json!({})),
            );
            assert_eq!(sync_retry_delay(&error, backoff).is_some(), retry);
        }
        let body = serde_json::from_value(serde_json::json!({ "errcode": "M_LIMIT_EXCEEDED", "error": "slow down", "retry_after_ms": 90000 })).unwrap();
        let error = error(429u16.try_into().unwrap(), ErrorBody::Standard(body));
        assert_eq!(
            sync_retry_delay(&error, backoff),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn event_cache_is_bounded_and_keeps_distinct_events() {
        let mut recent = RecentEvents::default();
        let first: OwnedEventId = "$first:example.org".try_into().unwrap();
        recent.remember(first.clone());
        recent.remember(first.clone());
        assert_eq!(recent.ids.len(), 1);
        for index in 0..RECENT_EVENTS {
            recent.remember(format!("${index}:example.org").try_into().unwrap());
        }
        assert_eq!(recent.ids.len(), RECENT_EVENTS);
        assert_eq!(recent.order.len(), RECENT_EVENTS);
        assert!(!recent.ids.contains(&first));
    }

    #[tokio::test]
    async fn shutdown_cancels_startup() {
        let mut config = config();
        config["matrix_server"] = "http://127.0.0.1:9".into();
        let bot = parse(&config).unwrap();
        timeout(Duration::from_secs(1), bot.run_until(async { Ok(()) }))
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(feature = "e2e-encryption")]
    #[tokio::test]
    async fn login_waits_for_encryption_initialization_before_returning() {
        let server = homeserver().await;
        let (requested, mut request_seen) = watch::channel(false);
        Mock::given(|request: &Request| {
            request
                .url
                .path()
                .ends_with("/m.secret_storage.default_key")
        })
        .respond_with(move |_: &Request| {
            requested.send_replace(true);
            ResponseTemplate::new(404)
                .set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "No secret storage"}))
                .set_delay(Duration::from_millis(250))
        })
        .with_priority(1)
        .mount(&server)
        .await;
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("state");
        let mut config = config();
        config["matrix_server"] = server.uri().into();
        config["matrix_user_id"] = USER.into();
        config["matrix_state_dir"] = directory.to_str().unwrap().into();
        let login = parse(&config).unwrap().login();
        tokio::pin!(login);
        timeout(Duration::from_secs(20), async {
            tokio::select! {
                result = &mut login => panic!("Login returned before encryption initialization: {result:?}"),
                result = request_seen.changed() => result.unwrap(),
            }
            // Keep the command alive while the SDK is awaiting the delayed response.
            assert!(timeout(Duration::from_millis(50), &mut login).await.is_err());
            login.await.unwrap();
        })
        .await
        .unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(directory.join("session.json")).unwrap()).unwrap();
        assert_eq!(saved["session"]["access_token"], "first-token");
    }

    #[cfg(not(feature = "e2e-encryption"))]
    #[tokio::test]
    async fn matrix_admission_preserves_event_time_and_drain_closes_live_senders() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::builder()
            .homeserver_url(format!("http://{}", listener.local_addr().unwrap()))
            .server_versions([MatrixVersion::V1_0])
            .request_config(RequestConfig::new().retry_limit(0))
            .build()
            .await
            .unwrap();
        let session: MatrixSession = serde_json::from_value(serde_json::json!({
            "user_id": "@bot:example.org", "device_id": "TEST", "access_token": "test-token"
        }))
        .unwrap();
        client
            .matrix_auth()
            .restore_session(session, Default::default())
            .await
            .unwrap();
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&request).contains("/sync"));
            let body = serde_json::json!({
                "next_batch": "next-token",
                "rooms": { "join": { "!test:example.org": {
                    "state": { "events": [{
                        "type": "m.room.member", "state_key": "@bot:example.org",
                        "sender": "@bot:example.org", "event_id": "$join:example.org",
                        "origin_server_ts": 1000, "content": { "membership": "join" }
                    }] },
                    "timeline": { "events": [], "limited": false, "prev_batch": "previous" }
                } } }
            })
            .to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(5), async {
            tokio::join!(client.sync_once(SyncSettings::default()), server)
        })
        .await
        .unwrap();
        result.unwrap();
        let room = client
            .get_room("!test:example.org".try_into().unwrap())
            .unwrap();
        let event = |msgtype: &str, body: String| -> OriginalSyncRoomMessageEvent {
            serde_json::from_value(serde_json::json!({
                "type": "m.room.message", "event_id": "$message:example.org",
                "sender": "@sender:example.org", "origin_server_ts": 1234567,
                "content": { "msgtype": msgtype, "body": body }
            }))
            .unwrap()
        };
        let counters = Counters::default();
        let queued =
            prepare_message(event("m.text", "hello".into()), room.clone(), &counters).unwrap();
        assert_eq!(queued.timestamp, 1234);
        assert_eq!(queued.text, "hello");
        assert!(
            prepare_message(event("m.notice", "ignored".into()), room.clone(), &counters).is_none()
        );
        assert!(
            prepare_message(
                event("m.text", "x".repeat(MAX_MESSAGE_BYTES + 1)),
                room,
                &counters
            )
            .is_none()
        );
        assert_eq!(counters.oversized.load(Ordering::Relaxed), 1);

        // An open sender must not keep shutdown waiting forever, as the old
        // process-global sender did. No database connection is needed here.
        let db = start_db("postgres://localhost:1/url?sslmode=disable")
            .await
            .unwrap();
        let (tx, rx) = mpsc::channel(1);
        let (drain, requested) = watch::channel(false);
        drain.send(true).unwrap();
        timeout(
            Duration::from_secs(1),
            handle_messages(rx, &Regex::new("(url)").unwrap(), &db, &counters, requested),
        )
        .await
        .unwrap();
        assert!(tx.is_closed());
    }
}

# sjmb_matrix

`sjmb_matrix` is a Matrix bot written in Rust that listens to room message events, extracts URLs with a configurable regex, and writes them into PostgreSQL.

## What the program does

- Uses your existing Matrix account with one persistent device and session.
- Supports end-to-end encrypted rooms by default, retaining encryption keys across restarts.
- Syncs joined Matrix rooms and receives message events.
- Filters for text messages.
- Extracts URLs from message text using a configured regular expression.
- Inserts detected URLs into a PostgreSQL `url` table.

## Project layout

- `src/bin/sjmb_matrix.rs`: executable entrypoint.
- `src/config.rs`: CLI flags, config path expansion, tracing setup.
- `src/matrixbot.rs`: bot config model, Matrix login/sync setup, event handlers, URL extraction flow.
- `src/db_util.rs`: PostgreSQL connection pool, bounded inserts and URL lookups.
- `src/harvest.rs`: URL validation, input limits, counters and console text sanitization.
- `src/runtime.rs`: scoped sync/worker supervision and shutdown handling.
- `src/session.rs`: private session files, SQLite store, account/device binding and token persistence.
- `src/verification.rs`: interactive verification with another device on your account.
- `src/encrypted.rs`: bounded retries for live encrypted events waiting for keys.
- `src/str_util.rs`: whitespace normalization helper for Matrix room and member names.
- `build.rs`: injects build metadata (`GIT_BRANCH`, `GIT_COMMIT`, `SOURCE_TIMESTAMP`, `RUSTC_VERSION`).
- `config/sjmb_matrix.json`: example runtime config.
- `install.sh`: copies the release binary to `$HOME/sjmb_matrix/bin`.
- `rust-toolchain.toml`: selects the stable Rust toolchain for local builds.

## Runtime configuration

The bot reads a JSON file. The default path is `$HOME/sjmb_matrix/config/sjmb_matrix.json`.

```json
{
  "url_regex": "<(https?://[^>]+)>",
  "url_log_db": "postgres:///url",
  "matrix_server": "https://example.org",
  "matrix_user_id": "@exampleuser:example.org",
  "matrix_state_dir": "$HOME/.local/state/sjmb-matrix"
}
```

Fields:

- `url_regex`: regex used to find URLs in message text. Capture group `1` is required; optional groups that did not match are skipped. Captures must be HTTP/HTTPS URLs with a host and no whitespace or control characters. Stored URL text is preserved exactly, including Unicode and query strings.
- `url_log_db`: PostgreSQL connection string used by `sqlx`.
- `matrix_server`: Matrix homeserver URL.
- `matrix_user_id`: your existing Matrix account, preferably its full `@user:server` ID. Existing short usernames are also supported. Keep this spelling unchanged after initializing state.
- `matrix_state_dir`: persistent device state directory, supporting environment/tilde expansion. Defaults to `$HOME/.local/state/sjmb-matrix`.
- `matrix_password`: optional password used only by `--login`. If omitted, login prompts with echo disabled. Normal startup and verification never use this field.

Configuration is limited to 1 MiB and validated before network access. The account ID must be nonempty; a password, when supplied, must be nonempty;
the homeserver must be an HTTP/HTTPS URL without embedded credentials, query parameters or a fragment. Use HTTPS
for remote homeservers: HTTP is retained for local or otherwise trusted deployments and sends credentials unencrypted.
Keep the populated config outside version control with permissions restricted to the operating-system account running the service.

Database TLS policy follows the connection string's `sslmode`; the bot does not disable TLS. For a local connection
where TLS is deliberately unnecessary, append `?sslmode=disable` (or `&sslmode=disable` after existing options).

## Matrix setup

Use your existing Matrix account; no separate bot account is needed. The bot monitors joined rooms, including
encrypted rooms and your own messages. End-to-end encryption is enabled in the default build.

Build, stop the old service, and initialize the new persistent device once:

```sh
cargo build --release
target/release/sjmb_matrix --bot-config /path/to/config.json --login
```

This creates a device named `sjmb-matrix` and saves its session and SQLite store. A failed/interrupted login can be
retried with `--login`; the chosen device ID and store are retained. Running `--login` on existing state explicitly
reauthenticates that same device. You can remove `matrix_password` from the config after setup, or omit it entirely
and use the password prompt.

Find your existing desktop session's device ID in that client's session/security settings. With the bot service
still stopped, request verification with that specific device on the same account:

```sh
target/release/sjmb_matrix --bot-config /path/to/config.json --verify DESKTOP_DEVICE_ID
```

Accept the request in your desktop client. Compare all seven emojis displayed on both sides, type `yes` in the
terminal only if they match, and confirm in the desktop client. Anything other than `yes` rejects the comparison.
Verification requires an interactive terminal, expires after three minutes, and ends on cancellation. The command
only initiates verification with the selected own-account device; the normal service never accepts unsolicited
verification requests. Existing cross-signing identities and backups are not reset or automatically created.

Start the service normally:

```sh
target/release/sjmb_matrix --verbose --bot-config /path/to/config.json
```

Normal startup restores the saved device, tokens and encryption keys. Its first sync establishes a current baseline:
room state and key exchanges are processed, while that sync's room-message backlog is not harvested. Subsequent
syncs process live messages and normal reconnect catch-up. No explicit history pagination or backup-key recovery is
performed. URLs sent while the process is stopped may be skipped, consistent with best-effort harvesting.

The state directory is private (`0700` on Unix), and the credential file and lock are `0600`. Only one process may
own it; stop the service before `--login` or `--verify`. The directory binds one configured account, homeserver,
device and encryption build setting. A mismatch, corrupt credential file, or missing store stops startup instead of
silently creating another device. Revoked credentials require an explicit `--login`. Token refreshes are saved
atomically; persistence failure stops the running service.

Keep the **whole state directory** across restarts. It contains account access/refresh tokens, the local store
passphrase, and the encrypted SDK SQLite store. The passphrase is stored alongside the credentials, so encryption
of SQLite is not protection against someone who can read the entire directory. Back up the directory while the
service is stopped if you want to preserve this device after disk loss. Losing it requires deliberate device setup
and verification again. The local store passphrase is separate from your Matrix account recovery key.

Old bot sessions created before this change remain in your account. After verifying the new persistent device,
you can remove those old sessions in your Matrix client. Do not remove your active desktop or new bot session.

HTTPS/TLS protects the connection independently of room E2EE. The bot decrypts messages locally and writes harvested
URLs into ordinary PostgreSQL text columns; INFO logging also includes plaintext message content.

For a plaintext-only build use `cargo build --release --no-default-features`. It has no verification command support
and must use a separate state directory from an E2EE-enabled device.

## Database expectations

The code expects an existing PostgreSQL database with these tables:

```sql
create table url (
    id bigserial primary key,
    seen bigint not null,
    channel text not null,
    nick text not null,
    url text not null
);
```

`db_add_url()` only inserts into `url`. Database change notifications are handled by PostgreSQL triggers managed by `urlharvest-rs`; this bot does not require or update the legacy `url_changed` table.

Room labels are stored as `matrix-{room_name}` after whitespace is converted to underscores. Sender display names receive the same whitespace normalization.
Unavailable, empty or control-containing names fall back to the room ID or sender's Matrix ID, keeping attribution
distinct when metadata is unavailable. `seen` stores the Matrix event's original timestamp in whole seconds.

## CLI flags

- `-v`, `--verbose`: `INFO` logs.
- `-d`, `--debug`: `DEBUG` logs.
- `-t`, `--trace`: `TRACE` logs.
- `--login`: initialize or explicitly reauthenticate the saved Matrix device.
- `--verify <DEVICE_ID>`: interactively verify with a desktop device on your account.
- `-b`, `--bot-config <PATH>`: config JSON path. Environment expansion is supported, for example `$HOME/...`.

If none of `verbose`, `debug`, or `trace` are set, log level defaults to `ERROR`. The built-in filter applies the selected level to `sjmb_matrix` and keeps dependency logs at `WARN` when verbose logging is enabled. Set `RUST_LOG` to override this.

Application message output is reduced to a single line and bounded to 4 KiB; room and sender console labels are
bounded separately. Bot `Debug` output redacts credentials and the database URL. Message bodies and URLs can still
contain private information, and overriding dependency log levels with `RUST_LOG` can expose SDK payloads.

## Program internals

### Startup sequence

1. `main` parses `OptsCommon` with `clap`.
2. `finish()` expands the `bot_config` path with `shellexpand`.
3. `start_pgm()` initializes tracing and logs build metadata from `build.rs` environment variables.
4. `Bot::new()` loads and validates JSON config, expands `url_log_db` and `matrix_state_dir`, and compiles `url_regex` locally.
5. `Bot::run()` creates a lazy database pool, locks the private state directory, restores the SDK client/session and performs a baseline sync within a 30-second startup budget.
6. A scoped event handler forwards messages to a worker while the client syncs. There is no process-global bot state.

Matrix requests have a 15-second timeout, extended by the SDK for sync long polling. The SDK handles request retries;
after these are exhausted, network failures, HTTP 408/429 and server errors restart sync using the same client and
its latest sync token. Delays grow from one second to one minute; longer server `Retry-After` values are honored.
A sync lasting at least a minute resets the delay. Permanent errors, including rejected credentials, exit with an error.
Startup failures also exit, allowing the service manager to apply its restart policy.

### Event and message flow

1. `Bot::run()` creates a bounded Tokio MPSC channel holding at most 256 messages.
2. Callbacks ignore non-joined rooms and non-text messages, reject bodies over 64 KiB, and queue only the needed
   body, IDs, timestamp and room handle. They wait for capacity when full, applying backpressure to Matrix sync.
3. One worker processes messages serially. Successful event IDs are remembered in a bounded cache of 8,192 entries
   to suppress repeat delivery during this run. Distinct events posting the same URL remain distinct.
4. The worker logs the room, sender and body, extracts URLs, and writes each URL to PostgreSQL.
5. Encrypted events that could not initially be decrypted are retained in memory: at most 128 events, at most
   128 KiB of ciphertext JSON each, for up to two minutes after receipt. The SDK is retried on room-key notifications
   and a periodic timer, at most 16 events per retry batch. Successfully decrypted text enters the same message
   filtering and duplicate-suppression path. Overflow evicts the oldest pending event; expiry/overflow/oversize
   discards are counted. Pending ciphertext is discarded on shutdown rather than delaying exit for keys.

Input discard counters are reported every 30 seconds at ERROR and on exit; duplicate counts are reported at DEBUG.
The queue bounds application backlog, not the SDK's HTTP response buffers or pending callbacks for the current sync batch.
SIGINT/SIGTERM cancels startup or stops sync, closes queue admission, and gives the worker up to ten seconds to drain.
A missed drain deadline is reported as an error. Cancelling the bot also cancels its worker; no detached worker survives.

### URL extraction and DB writes

`handle_msg()`:

1. Resolves the Matrix sender display name with `room.get_member()`, with a three-second deadline and ID fallback.
2. Resolves the room name with `room.name()`, falling back to its ID.
3. Trims the text message body.
4. Examines at most 32 `url_regex` matches. Invalid captures and URLs exceeding 8 KiB are skipped whole.
5. Reuses the runtime database pool for every insert.
6. Calls `db_add_url()` with the event timestamp, channel, nick, and URL. An insert failure is logged and does not
   prevent attempts to store the message's remaining URLs.

`db_add_url()` behavior:

- Executes `insert into url (seen, channel, nick, url) values (...)`.
- Uses a lazy pool of at most four connections with a three-second acquisition deadline. Database unavailability
  does not prevent Matrix startup.
- Sets five-second statement and one-second lock deadlines on each PostgreSQL connection, with an eight-second
  overall insert deadline. The exported URL lookup helper has the same overall deadline and returns `None` on no matches.
- Does not retry inserts automatically: a lost reply can follow a committed write, so retrying without an
  idempotency key can duplicate rows. PostgreSQL errors expose SQLSTATE rather than potentially private row details.

Harvesting remains best effort. Database failures, input limits, crashes and a shutdown deadline can lose URLs.
Duplicate suppression is in memory and only records wholly successful messages; a partial failure followed by
redelivery can repeat earlier successful inserts. Persistent SDK sync progress is not a database acknowledgement;
a crash can lose queued URLs even though the Matrix session resumes successfully. There is no durable harvesting
queue, history backfill or exactly-once guarantee. URLs are stored without being fetched; downstream consumers
remain responsible for safe fetching and rendering.

See [ROBUSTNESS_REVIEW.md](ROBUSTNESS_REVIEW.md) for the sibling-project comparison and remaining design questions.

## Building and installing

This project tracks the stable Rust toolchain through `rust-toolchain.toml`.

Build and verify:

```sh
cargo fmt --check
cargo test
cargo test --no-default-features
cargo clippy --all-targets -- -D warnings
cargo check --all-features
cargo build --release
```

The PostgreSQL integration test is ignored by default. Run it against a disposable database; it creates temporary
tables/functions and checks real inserts, empty lookups, error redaction and server statement deadlines:

```sh
SJMB_MATRIX_TEST_DATABASE_URL='postgres://user@localhost/disposable_db' \
  cargo test postgres_inserts_empty_lookups_errors_and_statement_deadlines -- --ignored
```

Install the release binary with:

```sh
./install.sh
```

The install script copies `target/release/sjmb_matrix` to `$HOME/sjmb_matrix/bin`.

## Dependency maintenance

Direct dependencies are listed explicitly in `Cargo.toml`. The Matrix SDK is intentionally pulled from the upstream git repository with `bundled-sqlite` enabled, so keep that storage feature unless system SQLite is deliberately reintroduced.

Useful dependency checks:

```sh
cargo update --dry-run
cargo outdated --root-deps-only
```

When updating `sqlx`, keep the Tokio runtime and Rustls TLS features enabled alongside PostgreSQL support.

## License

This project is licensed under `MIT OR Apache-2.0`.

# sjmb_matrix

`sjmb_matrix` is a Matrix bot written in Rust that listens to room message events, extracts URLs with a configurable regex, and writes them into PostgreSQL.

## What the program does

- Connects to a Matrix homeserver with username/password login.
- Syncs joined Matrix rooms and receives message events.
- Filters for text messages.
- Extracts URLs from message text using a configured regular expression.
- Inserts detected URLs into a PostgreSQL `url` table.
- Updates a `url_changed` marker table after inserts.

## Project layout

- `src/bin/sjmb_matrix.rs`: executable entrypoint.
- `src/config.rs`: CLI flags, config path expansion, tracing setup.
- `src/matrixbot.rs`: bot config model, Matrix login/sync setup, event handlers, URL extraction flow.
- `src/db_util.rs`: PostgreSQL connection helpers and insert retry logic.
- `src/str_util.rs`: whitespace normalization helper for Matrix room and member names.
- `build.rs`: injects build metadata (`GIT_BRANCH`, `GIT_COMMIT`, `SOURCE_TIMESTAMP`, `RUSTC_VERSION`).
- `config/sjmb_matrix.json`: example runtime config.
- `install.sh`: copies the release binary to `$HOME/sjmb_matrix/bin`.

## Runtime configuration

The bot reads a JSON file. The default path is `$HOME/sjmb_matrix/config/sjmb_matrix.json`.

```json
{
  "url_regex": "<(https?://[^>]+)>",
  "url_log_db": "postgres:///url",
  "matrix_server": "https://example.org",
  "matrix_user_id": "exampleuser",
  "matrix_password": "xyzzy"
}
```

Fields:

- `url_regex`: regex used to find URLs in message text. The bot stores capture group `1`.
- `url_log_db`: PostgreSQL connection string used by `sqlx`.
- `matrix_server`: Matrix homeserver URL.
- `matrix_user_id`: Matrix username used for password login.
- `matrix_password`: Matrix password for the bot account.

## Matrix setup

Create a dedicated Matrix account for the bot, then invite it to the rooms you want monitored. The bot only processes rooms where its membership state is `Joined`.

The bot currently uses password login:

1. Create the bot account on the homeserver.
2. Put the homeserver URL, username, and password in `config/sjmb_matrix.json`.
3. Invite the bot account to each room to monitor.
4. Accept joins as needed from the bot account or homeserver UI.
5. Run the bot and confirm it logs incoming text messages at `INFO` level.

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

create table url_changed (
    last bigint not null
);
```

`db_add_url()` inserts into `url` and then updates `url_changed.last`. Ensure `url_changed` has at least one row before running the bot.

Room labels are stored as `matrix-{room_name}` after whitespace is converted to underscores. Sender display names receive the same whitespace normalization.

## CLI flags

- `-v`, `--verbose`: `INFO` logs.
- `-d`, `--debug`: `DEBUG` logs.
- `-t`, `--trace`: `TRACE` logs.
- `-b`, `--bot-config <PATH>`: config JSON path. Environment expansion is supported, for example `$HOME/...`.

If none of `verbose`, `debug`, or `trace` are set, log level defaults to `ERROR`.

## Program internals

### Startup sequence

1. `main` parses `OptsCommon` with `clap`.
2. `finish()` expands the `bot_config` path with `shellexpand`.
3. `start_pgm()` initializes tracing and logs build metadata from `build.rs` environment variables.
4. `Bot::new()` loads JSON config, expands `url_log_db`, compiles `url_regex`, builds a Matrix client, and logs in.
5. `Bot::run()` registers the Matrix event handler and starts syncing.

### Event and message flow

1. `Bot::run()` creates an unbounded Tokio MPSC channel for message processing.
2. Matrix event callbacks forward `OriginalSyncRoomMessageEvent` values into that channel.
3. `handle_messages()` serially processes queued events.
4. `handle_msg()` ignores non-joined rooms and non-text messages.
5. For text messages, the bot logs the room, sender, and body, extracts URLs, and writes each URL to PostgreSQL.

### URL extraction and DB writes

`handle_msg()`:

1. Resolves the Matrix sender display name with `room.get_member()`.
2. Resolves the room name with `room.name()`.
3. Trims the text message body.
4. Runs `url_regex` captures over the text.
5. For each URL capture, opens a DB pool with `start_db(url_log_db)`.
6. Calls `db_add_url()` with timestamp, channel, nick, and URL.

`db_add_url()` behavior:

- Executes `insert into url (seen, channel, nick, url) values (...)`.
- Retries up to `RETRY_CNT = 5` with `RETRY_SLEEP = 1s` on failure.
- Calls `db_mark_change()` (`update url_changed set last = $1`) if `update_change` is true.

## Building and installing

Build and verify:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Install the release binary with:

```sh
./install.sh
```

The install script copies `target/release/sjmb_matrix` to `$HOME/sjmb_matrix/bin`.

## License

This project is licensed under `MIT OR Apache-2.0`.

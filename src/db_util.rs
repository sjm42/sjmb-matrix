use anyhow::{Context, anyhow, ensure};
use chrono::Utc;
use sqlx::{
    Pool, Postgres,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::time::{Duration, timeout};

const DB_JOB_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct DbCtx {
    pub dbc: Pool<Postgres>,
}

#[derive(Debug)]
pub struct UrlCtx {
    pub ts: i64,
    pub chan: String,
    pub nick: String,
    pub url: String,
}

pub async fn start_db<S: AsRef<str>>(db_url: S) -> anyhow::Result<DbCtx> {
    let options = db_url
        .as_ref()
        .parse::<PgConnectOptions>()
        .map_err(|_| anyhow!("Invalid database connection options"))?
        .options([("statement_timeout", "5000"), ("lock_timeout", "1000")]);
    // Preserve the URL's sslmode. Startup does not depend on DB availability.
    let dbc = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(3))
        .connect_lazy_with(options);
    Ok(DbCtx { dbc })
}

const SQL_INSERT_URL: &str = "insert into url \
    (seen, channel, nick, url) \
    values ($1, $2, $3, $4)";

// PostgreSQL error DETAIL can contain whole records or authentication details.
// Keep the SQLSTATE for diagnostics without putting row contents in logs.
fn database_error(error: sqlx::Error) -> anyhow::Error {
    match error {
        sqlx::Error::Database(error) => anyhow!(
            "PostgreSQL operation failed (SQLSTATE {})",
            error.code().as_deref().unwrap_or("unknown")
        ),
        sqlx::Error::Io(_) => anyhow!("Database connection I/O failed"),
        sqlx::Error::Tls(_) => anyhow!("Database TLS negotiation failed"),
        other => anyhow!(other),
    }
}

pub async fn db_add_url(db: &DbCtx, ur: &UrlCtx) -> anyhow::Result<u64> {
    ensure!(
        ur.url.len() <= crate::harvest::MAX_URL_BYTES,
        "URL exceeds 8 KiB"
    );
    ensure!(
        ![ur.chan.as_str(), ur.nick.as_str(), ur.url.as_str()]
            .iter()
            .any(|value| value.contains('\0')),
        "URL record contains a NUL byte unsupported by PostgreSQL text"
    );
    // A lost reply may follow a committed INSERT. Retrying it blindly can
    // duplicate rows because this schema has no event ID / idempotency key.
    timeout(DB_JOB_TIMEOUT, async {
        sqlx::query(SQL_INSERT_URL)
            .bind(ur.ts)
            .bind(&ur.chan)
            .bind(&ur.nick)
            .bind(&ur.url)
            .execute(&db.dbc)
            .await
            .map(|result| result.rows_affected())
            .map_err(database_error)
    })
    .await
    .context("URL insert timed out; commit status may be unknown")?
}

#[derive(Debug, sqlx::FromRow)]
pub struct CheckUrl {
    pub cnt: i64,
    pub min: i64,
    pub max: i64,
}

// HAVING avoids decoding NULL min/max when the aggregate has no matches.
const SQL_CHECK_URL: &str = "select count(id) as cnt, min(seen) as min, max(seen) as max \
     from url where url = $1 and channel = $2 and seen > $3 having count(id) > 0";

pub async fn db_check_url(
    db: &DbCtx,
    url: &str,
    chan: &str,
    expire_s: i64,
) -> anyhow::Result<Option<CheckUrl>> {
    ensure!(expire_s >= 0, "URL expiry must not be negative");
    let since = Utc::now()
        .timestamp()
        .checked_sub(expire_s)
        .context("URL expiry overflow")?;
    timeout(
        DB_JOB_TIMEOUT,
        sqlx::query_as::<_, CheckUrl>(SQL_CHECK_URL)
            .bind(url)
            .bind(chan)
            .bind(since)
            .fetch_optional(&db.dbc),
    )
    .await
    .context("URL lookup timed out")?
    .map_err(database_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncReadExt, net::TcpListener};

    #[tokio::test]
    #[ignore = "requires SJMB_MATRIX_TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
    async fn postgres_inserts_empty_lookups_errors_and_statement_deadlines() {
        let db = start_db(std::env::var("SJMB_MATRIX_TEST_DATABASE_URL").unwrap())
            .await
            .unwrap();
        // TEMP objects belong to one session. Keep this integration test on a
        // single connection while retaining the production startup options.
        let db = DbCtx {
            dbc: PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy_with(db.dbc.connect_options().as_ref().clone()),
        };
        sqlx::query("create temporary table url (id bigserial primary key, seen bigint not null, channel text not null, nick text not null, url text not null)")
            .execute(&db.dbc).await.unwrap();
        assert!(
            db_check_url(&db, "https://example.org/é", "matrix-test", 60)
                .await
                .unwrap()
                .is_none()
        );
        let row = UrlCtx {
            ts: Utc::now().timestamp(),
            chan: "matrix-test".into(),
            nick: "sender".into(),
            url: "https://example.org/é".into(),
        };
        assert_eq!(db_add_url(&db, &row).await.unwrap(), 1);
        let found = db_check_url(&db, &row.url, &row.chan, 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.cnt, 1);
        assert_eq!((found.min, found.max), (row.ts, row.ts));
        sqlx::query("create temporary sequence insert_attempts")
            .execute(&db.dbc)
            .await
            .unwrap();
        sqlx::query("create function pg_temp.reject_url() returns trigger language plpgsql as $$ begin perform nextval('insert_attempts'); raise exception 'private row contents' using errcode = '23514'; end $$")
            .execute(&db.dbc).await.unwrap();
        sqlx::query("create trigger reject_url before insert on url for each row execute function pg_temp.reject_url()")
            .execute(&db.dbc).await.unwrap();
        let error = db_add_url(&db, &row).await.unwrap_err();
        assert!(error.to_string().contains("23514"));
        assert!(!format!("{error:#}").contains("private row contents"));
        let attempts: i64 = sqlx::query_scalar("select last_value from insert_attempts")
            .fetch_one(&db.dbc)
            .await
            .unwrap();
        assert_eq!(attempts, 1);
        sqlx::query("drop trigger reject_url on url")
            .execute(&db.dbc)
            .await
            .unwrap();
        sqlx::query("create function pg_temp.slow_url() returns trigger language plpgsql as $$ begin perform pg_sleep(10); return new; end $$")
            .execute(&db.dbc).await.unwrap();
        sqlx::query("create trigger slow_url before insert on url for each row execute function pg_temp.slow_url()")
            .execute(&db.dbc).await.unwrap();
        let started = tokio::time::Instant::now();
        let error = db_add_url(&db, &row).await.unwrap_err();
        assert!(error.to_string().contains("57014"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(7));
        let count: i64 = sqlx::query_scalar("select count(*) from url")
            .fetch_one(&db.dbc)
            .await
            .unwrap();
        assert_eq!(count, 1);
        db.dbc.close().await;
    }

    #[tokio::test]
    async fn database_setup_is_lazy_and_invalid_settings_fail_locally() {
        let db = start_db("postgres://localhost:1/url?sslmode=disable")
            .await
            .unwrap();
        assert_eq!(db.dbc.size(), 0);
        assert!(start_db("not a database URL").await.is_err());
        assert!(start_db("postgres:///url?sslmode=invalid").await.is_err());
        let mut record = UrlCtx {
            ts: 1,
            chan: "test".into(),
            nick: "sender".into(),
            url: "a".repeat(crate::harvest::MAX_URL_BYTES + 1),
        };
        assert!(
            db_add_url(&db, &record)
                .await
                .unwrap_err()
                .to_string()
                .contains("8 KiB")
        );
        record.url = "https://example.org/".into();
        record.nick = "bad\0nick".into();
        assert!(
            db_add_url(&db, &record)
                .await
                .unwrap_err()
                .to_string()
                .contains("NUL")
        );
        assert!(db_check_url(&db, "url", "channel", -1).await.is_err());
        assert_eq!(db.dbc.size(), 0);
    }

    #[tokio::test]
    async fn stalled_postgres_handshake_is_bounded_and_sslmode_is_respected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let db = start_db(format!("postgres://user@{address}/url?sslmode=disable"))
            .await
            .unwrap();
        let record = UrlCtx {
            ts: 1,
            chan: "test".into(),
            nick: "sender".into(),
            url: "https://example.org/".into(),
        };
        let mut insert = Box::pin(db_add_url(&db, &record));
        let mut socket = tokio::select! {
            accepted = listener.accept() => accepted.unwrap().0,
            result = &mut insert => panic!("insert ended before connection: {result:?}"),
        };
        let mut header = [0; 8];
        tokio::select! {
            read = socket.read_exact(&mut header) => { read.unwrap(); },
            result = &mut insert => panic!("insert ended before handshake: {result:?}"),
        }
        // Protocol 3 startup, rather than an SSL negotiation request.
        assert_eq!(u32::from_be_bytes(header[4..8].try_into().unwrap()), 196608);
        assert!(
            timeout(Duration::from_secs(5), &mut insert)
                .await
                .unwrap()
                .is_err()
        );
        drop(insert);
        drop(socket);
        timeout(Duration::from_secs(2), db.dbc.close())
            .await
            .unwrap();
    }
}

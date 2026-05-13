// db_util.rs

use anyhow::Context;
use chrono::*;
use futures::TryStreamExt;
use sqlx::{Pool, Postgres};
use tokio::time::{Duration, sleep};

use crate::*;

const RETRY_CNT: usize = 5;
const RETRY_SLEEP: u64 = 1;

#[derive(Clone, Debug)]
pub struct DbCtx {
    pub dbc: Pool<Postgres>,
    pub update_change: bool,
}

#[derive(Debug)]
pub struct UrlCtx {
    pub ts: i64,
    pub chan: String,
    pub nick: String,
    pub url: String,
}

pub async fn start_db<S>(db_url: S) -> anyhow::Result<DbCtx>
where
    S: AsRef<str>,
{
    let dbc = sqlx::PgPool::connect(db_url.as_ref()).await?;
    let db = DbCtx {
        dbc,
        update_change: true,
    };
    Ok(db)
}

const SQL_UPDATE_CHANGE: &str = "update url_changed set last = $1";

pub async fn db_mark_change(dbc: &Pool<Postgres>) -> anyhow::Result<()> {
    sqlx::query(SQL_UPDATE_CHANGE)
        .bind(Utc::now().timestamp())
        .execute(dbc)
        .await?;
    Ok(())
}

const SQL_INSERT_URL: &str = "insert into url \
    (seen, channel, nick, url) \
    values ($1, $2, $3, $4)";

pub async fn db_add_url(db: &DbCtx, ur: &UrlCtx) -> anyhow::Result<u64> {
    for attempt in 1..=RETRY_CNT {
        match db_add_url_once(db, ur).await {
            Ok(rowcnt) => return Ok(rowcnt),
            Err(e) if attempt == RETRY_CNT => {
                return Err(e).with_context(|| {
                    format!(
                        "URL insert failed after {RETRY_CNT} attempts for channel {}",
                        ur.chan
                    )
                });
            }
            Err(e) => {
                warn!(
                    "URL insert attempt {attempt}/{RETRY_CNT} failed for channel {}: {e:#}",
                    ur.chan
                );
                sleep(Duration::new(RETRY_SLEEP, 0)).await;
            }
        }
    }

    unreachable!("retry loop always returns");
}

async fn db_add_url_once(db: &DbCtx, ur: &UrlCtx) -> anyhow::Result<u64> {
    let mut tx = db.dbc.begin().await?;

    let res = sqlx::query(SQL_INSERT_URL)
        .bind(ur.ts)
        .bind(&ur.chan)
        .bind(&ur.nick)
        .bind(&ur.url)
        .execute(&mut *tx)
        .await?;

    if db.update_change {
        sqlx::query(SQL_UPDATE_CHANGE)
            .bind(Utc::now().timestamp())
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(res.rows_affected())
}

#[derive(Debug, sqlx::FromRow)]
pub struct CheckUrl {
    pub cnt: i64,
    pub min: i64,
    pub max: i64,
}

const SQL_CHECK_URL: &str = "select count(id) as cnt, min(seen) as min, max(seen) as max \
     from url where url = $1 and channel = $2 and seen > $3";

pub async fn db_check_url(
    db: &DbCtx,
    url: &str,
    chan: &str,
    expire_s: i64,
) -> anyhow::Result<Option<CheckUrl>> {
    let mut st_check_url = sqlx::query_as::<_, CheckUrl>(SQL_CHECK_URL)
        .bind(url)
        .bind(chan)
        .bind(Utc::now().timestamp() - expire_s)
        .fetch(&db.dbc);
    Ok(st_check_url.try_next().await?)
}

// EOF

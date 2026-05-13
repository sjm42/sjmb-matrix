// slackbot.rs

use std::{fs::File, io::BufReader, sync::Arc};

use ::serde::{Deserialize, Serialize};
use anyhow::anyhow;
use chrono::*;
use matrix_sdk::{
    Client, RoomState,
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
};
use once_cell::sync::OnceCell;
use regex::Regex;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::*;

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Bot {
    url_regex: String,
    url_log_db: String,

    matrix_server: String,
    matrix_user_id: String,
    matrix_password: String,

    #[serde(skip)]
    url_re: Option<Regex>,
    #[serde(skip)]
    client: Option<Client>,
}

#[derive(Debug)]
struct BotState {
    bot: Arc<Bot>,
    db: DbCtx,
    tx: Sender<QueuedMessage>,
}

struct QueuedMessage {
    bot: Arc<Bot>,
    db: DbCtx,
    event: OriginalSyncRoomMessageEvent,
    room: Room,
}

static MY_BOT: OnceCell<BotState> = OnceCell::new();

impl Bot {
    pub async fn new(opts: &OptsCommon) -> anyhow::Result<Self> {
        let now1 = Utc::now();

        let file = &opts.bot_config;
        info!("Reading config file {file}");
        let mut bot: Bot = serde_json::from_reader(BufReader::new(File::open(file)?))?;

        // Expand $HOME where relevant
        bot.url_log_db = shellexpand::full(&bot.url_log_db)?.into_owned();

        // pre-compile url detection regex
        bot.url_re = Some(Regex::new(&bot.url_regex)?);

        let client = Client::builder()
            .homeserver_url(&bot.matrix_server)
            .build()
            .await?;

        client
            .matrix_auth()
            .login_username(&bot.matrix_user_id, &bot.matrix_password)
            .send()
            .await?;
        bot.client = Some(client);

        info!(
            "New runtime config successfully created in {} ms.",
            Utc::now().signed_duration_since(now1).num_milliseconds()
        );
        // debug!("New BotConfig:\n{bot:#?}");

        Ok(bot)
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let mut handles = vec![];
        let me = Arc::new(self);
        let bot = me.clone();
        let db = start_db(&bot.url_log_db).await?;
        let (tx, rx) = mpsc::channel::<QueuedMessage>(MESSAGE_QUEUE_BOUND);

        handles.push(tokio::spawn(async move { handle_messages(rx).await }));

        bot.client.as_ref().unwrap().add_event_handler(handle_event);
        MY_BOT.set(BotState { bot, db, tx }).ok();
        MY_BOT
            .get()
            .as_ref()
            .unwrap()
            .bot
            .client
            .as_ref()
            .unwrap()
            .sync(SyncSettings::default())
            .await?;

        futures::future::join_all(handles).await;
        Ok(())
    }
}

async fn handle_event(ev: OriginalSyncRoomMessageEvent, room: Room, _client: Client) {
    let bot = MY_BOT.get().unwrap();
    if bot
        .tx
        .send(QueuedMessage {
            bot: bot.bot.clone(),
            db: bot.db.clone(),
            event: ev,
            room,
        })
        .await
        .is_err()
    {
        error!("Matrix msg queue receiver is closed");
    }
}

async fn handle_messages(mut rx: Receiver<QueuedMessage>) {
    while let Some(queued) = rx.recv().await {
        // debug!("Got message: {msg:#?}");
        if let Err(e) = handle_msg(queued.bot, queued.db, queued.event, queued.room).await {
            error!("Matrix msg handling failed: {e:?}");
        }
    }
}

async fn handle_msg(
    bot: Arc<Bot>,
    db: DbCtx,
    event: OriginalSyncRoomMessageEvent,
    room: Room,
) -> anyhow::Result<()> {
    match room.state() {
        RoomState::Joined => {}
        _ => return Ok(()),
    }

    // We only want to log text messages.
    let msgtype = match &event.content.msgtype {
        MessageType::Text(msgtype) => msgtype,
        _ => return Ok(()),
    };

    let nick = match room.get_member(&event.sender).await {
        Ok(Some(m)) => m.name().to_string(),
        _ => "UNKNOWN".into(),
    }
    .ws_convert();

    let room_name = room.name().unwrap_or_else(|| "NONE".to_string());
    let room_name = room_name.ws_convert();
    let text = msgtype.body.trim();
    info!("#[{room_name}] <{nick}>: {text}",);

    for url_cap in bot
        .url_re
        .as_ref()
        .ok_or_else(|| anyhow!("No url_regex_re"))?
        .captures_iter(text)
    {
        let Some(url_match) = url_cap.get(1) else {
            error!("Configured url_regex matched without capture group 1");
            continue;
        };
        let url_s = url_match.as_str().to_string();
        info!("*** on {room_name} detected url: {url_s}");

        info!(
            "Urllog: inserted {} row(s)",
            db_add_url(
                &db,
                &UrlCtx {
                    ts: Utc::now().timestamp(),
                    chan: format!("matrix-{}", room_name),
                    nick: nick.clone(),
                    url: url_s,
                },
            )
            .await?
        );
    }
    Ok(())
}

// EOF

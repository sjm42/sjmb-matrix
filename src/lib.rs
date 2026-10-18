// lib.rs

#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
pub use std::{
    collections::{self, HashSet, VecDeque},
    env, fmt,
    fs::{self, File, OpenOptions},
    future::{self, Future},
    io::{self, Read, Write},
    mem,
    path::{self, Path, PathBuf},
    sync::{
        self, Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread, time,
};

pub use anyhow::{Context, anyhow, bail, ensure};
pub use chrono::Utc;
pub use clap::Parser;
#[cfg(feature = "e2e-encryption")]
pub use futures_util::StreamExt;
pub use matrix_sdk::{
    Client, HttpError, RoomState, SessionTokens,
    authentication::matrix::MatrixSession,
    config::{RequestConfig, SyncSettings},
    room::Room,
    ruma::{
        OwnedEventId, OwnedUserId,
        api::error::{ErrorKind, RetryAfter},
        events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
    },
};
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk::{
    encryption::verification::{
        SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
        format_emojis,
    },
    event_handler::RawEvent,
    ruma::{
        DeviceId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            key::verification::VerificationMethod,
            room::{encrypted::OriginalSyncRoomEncryptedEvent, message::SyncRoomMessageEvent},
        },
        serde::Raw,
    },
};
pub use rand::distr::{Alphanumeric, SampleString};
pub use regex::Regex;
pub use serde::{Deserialize, Serialize};
pub use sqlx::{
    Pool, Postgres,
    postgres::{PgConnectOptions, PgPoolOptions},
};
pub use tokio::{
    sync::{mpsc, watch},
    time::{Duration, Instant, interval, sleep, timeout},
};
pub use tracing::{Level, debug, error, info, trace, warn};
pub use tracing_subscriber::{EnvFilter, fmt as tracing_fmt};
pub use url::Url;

#[cfg(all(test, feature = "e2e-encryption"))]
pub use matrix_sdk::encryption::vodozemac::{
    Curve25519PublicKey, Curve25519SecretKey, Ed25519Keypair,
    megolm::{GroupSession, InboundGroupSession, SessionConfig},
};
#[cfg(all(test, not(feature = "e2e-encryption")))]
pub use matrix_sdk::ruma::api::MatrixVersion;
#[cfg(test)]
pub use matrix_sdk::ruma::api::{
    client::uiaa::UiaaResponse,
    error::{ErrorBody, FromHttpResponseError},
};
#[cfg(test)]
pub use serde_json::{Value, json};
#[cfg(feature = "e2e-encryption")]
pub use std::io::IsTerminal;
#[cfg(test)]
pub use std::{future::pending, sync::atomic::AtomicBool};
#[cfg(test)]
pub use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[cfg(test)]
pub use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::any};

pub const MESSAGE_QUEUE_BOUND: usize = 256;

pub use config::*;
pub use db_util::*;
pub use matrixbot::*;
pub use str_util::*;

pub(crate) use crate::{
    harvest::{Counters, MAX_MESSAGE_BYTES, extract_urls, log_text},
    runtime::{shutdown_signal, supervise},
    session::{SessionStore, password_prompt},
};
#[cfg(feature = "e2e-encryption")]
pub(crate) use matrixbot::{QueuedMessage, prepare_message, sync_client};
#[cfg(all(test, feature = "e2e-encryption"))]
pub(crate) use test_support::ROOM;
#[cfg(test)]
pub(crate) use test_support::{USER, homeserver};

pub mod config;
pub mod db_util;
#[cfg(feature = "e2e-encryption")]
mod encrypted;
mod harvest;
pub mod matrixbot;
mod runtime;
mod session;
pub mod str_util;
#[cfg(test)]
mod test_support;
#[cfg(feature = "e2e-encryption")]
mod verification;

// EOF

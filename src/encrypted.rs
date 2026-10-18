//! Short-lived retries for live ciphertext whose room key has not arrived yet.
use std::{collections::VecDeque, sync::atomic::Ordering};

use matrix_sdk::{
    Room, RoomState,
    ruma::{OwnedEventId, events::room::encrypted::OriginalSyncRoomEncryptedEvent, serde::Raw},
};
use tokio::time::{Duration, Instant, timeout};

use crate::{
    harvest::Counters,
    matrixbot::{QueuedMessage, prepare_message},
};

pub(crate) const MAX_CIPHERTEXT_BYTES: usize = 128 * 1024;
const PENDING_CAPACITY: usize = 128;
const KEY_WAIT: Duration = Duration::from_secs(120);

pub(crate) struct EncryptedMessage {
    pub(crate) event: Raw<OriginalSyncRoomEncryptedEvent>,
    pub(crate) room: Room,
    received: Instant,
    id: OwnedEventId,
}

impl EncryptedMessage {
    pub(crate) fn new(
        event: Raw<OriginalSyncRoomEncryptedEvent>,
        room: Room,
        id: OwnedEventId,
    ) -> Self {
        Self {
            event,
            room,
            id,
            received: Instant::now(),
        }
    }
}

#[derive(Default)]
pub(crate) struct PendingEvents(VecDeque<EncryptedMessage>);

impl PendingEvents {
    pub(crate) fn push(&mut self, message: EncryptedMessage, counters: &Counters) {
        if self.0.iter().any(|pending| pending.id == message.id) {
            return;
        }
        if self.0.len() == PENDING_CAPACITY {
            self.0.pop_front();
            counters.encrypted_discarded.fetch_add(1, Ordering::Relaxed);
        }
        self.0.push_back(message);
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) async fn retry_one(&mut self, counters: &Counters) -> Option<QueuedMessage> {
        let pending = self.0.pop_front()?;
        if pending.received.elapsed() >= KEY_WAIT || pending.room.state() != RoomState::Joined {
            counters.encrypted_discarded.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        match timeout(
            Duration::from_secs(3),
            pending.room.decrypt_event(&pending.event, None),
        )
        .await
        {
            Ok(Ok(event)) if !event.kind.is_utd() => {
                // Apply the same message/text filtering as ordinary callbacks.
                use matrix_sdk::ruma::events::{
                    AnySyncMessageLikeEvent, AnySyncTimelineEvent,
                    room::message::SyncRoomMessageEvent,
                };
                match event.raw().deserialize().ok()? {
                    AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                        SyncRoomMessageEvent::Original(event),
                    )) => prepare_message(event, pending.room, counters),
                    _ => None,
                }
            }
            _ => {
                self.0.push_back(pending);
                None
            }
        }
    }

    pub(crate) fn discard(self, counters: &Counters) {
        counters
            .encrypted_discarded
            .fetch_add(self.0.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::SessionStore,
        test_support::{ROOM, USER, homeserver},
    };
    use futures_util::StreamExt;
    use matrix_sdk::encryption::vodozemac::{
        Curve25519PublicKey, Curve25519SecretKey, Ed25519Keypair,
        megolm::{GroupSession, InboundGroupSession, SessionConfig},
    };
    use serde_json::json;

    #[tokio::test]
    async fn live_ciphertext_is_retried_after_key_arrival_and_survives_sdk_store_restore() {
        let server = homeserver().await;
        let root = tempfile::tempdir().unwrap();
        let state_path = root.path().join("state");
        let state =
            SessionStore::open(state_path.to_str().unwrap(), &server.uri(), USER, true).unwrap();
        let client = state.connect(Some("fake-password")).await.unwrap();
        client
            .sync_once(matrix_sdk::config::SyncSettings::default())
            .await
            .unwrap();
        let room = client.get_room(ROOM.try_into().unwrap()).unwrap();
        let mut outbound = GroupSession::new(SessionConfig::version_1());
        let inbound = InboundGroupSession::new(&outbound.session_key(), SessionConfig::version_1());
        let sender_key = Curve25519PublicKey::from(&Curve25519SecretKey::new()).to_base64();
        let signing_key = Ed25519Keypair::new().public_key().to_base64();
        let key = serde_json::from_value(json!({
            "algorithm": "m.megolm.v1.aes-sha2", "room_id": ROOM,
            "sender_key": sender_key, "session_id": outbound.session_id(),
            "session_key": inbound.export_at_first_known_index().to_base64(),
            "sender_claimed_keys": {"ed25519": signing_key}, "forwarding_curve25519_key_chain": []
        }))
        .unwrap();
        let ciphertext = outbound.encrypt(json!({"room_id": ROOM, "type": "m.room.message", "content": {"msgtype": "m.text", "body": "<https://example.org/live>"}}).to_string()).to_base64();
        let raw = Raw::from_json_string(json!({
            "type": "m.room.encrypted", "event_id": "$live:example.org", "sender": USER, "origin_server_ts": 1234567,
            "content": {"algorithm": "m.megolm.v1.aes-sha2", "sender_key": sender_key, "session_id": outbound.session_id(), "device_id": "DESKTOP", "ciphertext": ciphertext}
        }).to_string()).unwrap();
        let counters = Counters::default();
        let mut pending = PendingEvents::default();
        pending.push(
            EncryptedMessage::new(
                raw.clone(),
                room.clone(),
                "$live:example.org".try_into().unwrap(),
            ),
            &counters,
        );
        assert!(pending.retry_one(&counters).await.is_none());
        assert_eq!(pending.len(), 1);
        let mut keys = client
            .encryption()
            .room_keys_received_stream()
            .await
            .unwrap();
        let export =
            matrix_sdk_crypto::encrypt_room_key_export(&[key], "fixture-password", 1).unwrap();
        let path = root.path().join("fixture.keys");
        std::fs::write(&path, export).unwrap();
        client
            .encryption()
            .import_room_keys(path, "fixture-password")
            .await
            .unwrap();
        timeout(Duration::from_secs(1), keys.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(pending.retry_one(&counters).await.is_some());
        assert_eq!(pending.len(), 0);
        // Reopening the same SDK store can decrypt the already received key,
        // without network history or a new login/device.
        let restored = state.connect(None).await.unwrap();
        let restored_room = restored.get_room(ROOM.try_into().unwrap()).unwrap();
        let event = restored_room.decrypt_event(&raw, None).await.unwrap();
        assert!(!event.kind.is_utd());
        assert!(
            event
                .raw()
                .json()
                .get()
                .contains("https://example.org/live")
        );

        // Duplicates, expiry and capacity are bounded independently of keys.
        let mut expired = EncryptedMessage::new(
            raw.clone(),
            room.clone(),
            "$expired:example.org".try_into().unwrap(),
        );
        expired.received = Instant::now() - KEY_WAIT;
        pending.push(expired, &counters);
        assert!(pending.retry_one(&counters).await.is_none());
        assert_eq!(counters.encrypted_discarded.load(Ordering::Relaxed), 1);
        for index in 0..PENDING_CAPACITY + 1 {
            pending.push(
                EncryptedMessage::new(
                    raw.clone(),
                    room.clone(),
                    format!("${index}:example.org").try_into().unwrap(),
                ),
                &counters,
            );
        }
        assert_eq!(pending.len(), PENDING_CAPACITY);
        assert_eq!(counters.encrypted_discarded.load(Ordering::Relaxed), 2);
        pending.push(
            EncryptedMessage::new(raw, room, "$1:example.org".try_into().unwrap()),
            &counters,
        );
        assert_eq!(pending.len(), PENDING_CAPACITY);
    }
}

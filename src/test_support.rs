use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::any};

pub(crate) const USER: &str = "@me:example.org";
pub(crate) const ROOM: &str = "!live:example.org";

pub(crate) async fn homeserver() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(|request: &Request| {
        let path = request.url.path();
        let body = if path.ends_with("/versions") {
            json!({ "versions": ["v1.11"] })
        } else if path.ends_with("/login") {
            let body: Value = request.body_json().unwrap();
            json!({"user_id": USER, "device_id": body["device_id"], "access_token": "first-token", "refresh_token": "first-refresh"})
        } else if path.ends_with("/refresh") {
            json!({"access_token": "refreshed-token", "refresh_token": "refreshed-refresh"})
        } else if path.ends_with("/keys/upload") {
            json!({"one_time_key_counts": {"signed_curve25519": 50}})
        } else if path.ends_with("/keys/query") {
            json!({"device_keys": {}})
        } else if path.ends_with("/sync") {
            sync_body()
        } else {
            return ResponseTemplate::new(404).set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "test endpoint not configured"}));
        };
        ResponseTemplate::new(200).set_body_json(body)
    }).mount(&server).await;
    server
}

pub(crate) fn sync_body() -> Value {
    json!({
        "next_batch": "live-token",
        "rooms": {"join": {ROOM: {
            "state": {"events": [
                {"type": "m.room.member", "state_key": USER, "sender": USER,
                 "event_id": "$join:example.org", "origin_server_ts": 1000, "content": {"membership": "join"}},
                {"type": "m.room.encryption", "state_key": "", "sender": USER,
                 "event_id": "$encryption:example.org", "origin_server_ts": 1000,
                 "content": {"algorithm": "m.megolm.v1.aes-sha2"}}
            ]},
            "timeline": {"events": [], "limited": false, "prev_batch": "previous"}
        }}}
    })
}

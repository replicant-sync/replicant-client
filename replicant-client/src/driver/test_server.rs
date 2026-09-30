//! A scripted protocol v2 server for engine tests: one in-memory document model answers the
//! join, feeds, uploads and fetches, and every commit is pushed to each joined socket. Knobs
//! lose, hold, reject or drop single replies and whole connections.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::engine::hash::{canonicalise_numbers, content_hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    /// Completes the websocket upgrade, then closes the connection at once.
    DropAfterUpgrade,
    /// Completes the upgrade and records frames but never answers one.
    SilentAfterUpgrade,
}

#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub connection: usize,
    pub event: String,
    pub payload: Value,
}

#[derive(Default)]
pub(crate) struct Stats {
    upgrades: AtomicUsize,
    live: AtomicUsize,
    peak_live: AtomicUsize,
}

impl Stats {
    pub fn upgrades(&self) -> usize {
        self.upgrades.load(Ordering::SeqCst)
    }

    /// Upgraded connections the server has not yet seen end.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    pub fn peak_live(&self) -> usize {
        self.peak_live.load(Ordering::SeqCst)
    }

    fn opened(&self) {
        self.upgrades.fetch_add(1, Ordering::SeqCst);
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_live.fetch_max(live, Ordering::SeqCst);
    }

    fn closed(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ServerDoc {
    /// As uploaded: integral floats stay floats.
    content: Value,
    seq: i64,
    deleted: bool,
}

struct LogEntry {
    seq: i64,
    prev_seq: i64,
    doc_id: Uuid,
    deleted: bool,
    upload_id: Option<Uuid>,
}

struct Held {
    connection: usize,
    join_ref: Value,
    reference: Value,
    payload: Value,
}

#[derive(Default)]
struct Outcome {
    replies: Vec<String>,
    close: bool,
}

struct UploadResult {
    reply: Option<(&'static str, Value)>,
    close: bool,
}

struct Model {
    mode: Mode,
    user_id: Uuid,
    join_error: Option<String>,
    clock_ahead: Option<i64>,
    seq: i64,
    docs: HashMap<Uuid, ServerDoc>,
    log: Vec<LogEntry>,
    trimmed_through: i64,
    replies: HashMap<(Uuid, Option<String>), Value>,
    hold_uploads: bool,
    held: Vec<Held>,
    lose_next_upload: bool,
    drop_after_next_upload: bool,
    reject_next_upload: Option<String>,
    drop_next_push: bool,
    frames: Vec<Frame>,
    user_agents: Vec<Option<String>>,
    outbound: Vec<mpsc::UnboundedSender<String>>,
    joined: HashSet<usize>,
}

pub(crate) struct ScriptedServer {
    pub url: String,
    pub stats: Arc<Stats>,
    model: Arc<Mutex<Model>>,
    kill: watch::Sender<u64>,
}

impl ScriptedServer {
    pub async fn start(user_id: Uuid) -> ScriptedServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let model = Arc::new(Mutex::new(Model::new(user_id)));
        let stats = Arc::new(Stats::default());
        let (kill, kill_rx) = watch::channel(0u64);
        tokio::spawn(accept_loop(listener, model.clone(), stats.clone(), kill_rx));
        ScriptedServer {
            url,
            stats,
            model,
            kill,
        }
    }

    fn model(&self) -> MutexGuard<'_, Model> {
        self.model.lock().unwrap()
    }

    pub fn set_mode(&self, mode: Mode) {
        self.model().mode = mode;
    }

    pub fn reject_join(&self, code: &str) {
        self.model().join_error = Some(code.to_string());
    }

    pub fn accept_joins(&self) {
        self.model().join_error = None;
    }

    /// Joins are checked against a server clock `secs` ahead of this machine's, with the real
    /// server's 300 s window.
    pub fn clock_ahead_by(&self, secs: i64) {
        self.model().clock_ahead = Some(secs);
    }

    pub fn lose_next_upload(&self) {
        self.model().lose_next_upload = true;
    }

    pub fn drop_after_next_upload(&self) {
        self.model().drop_after_next_upload = true;
    }

    pub fn hold_uploads(&self) {
        self.model().hold_uploads = true;
    }

    /// Stops holding and processes every held upload in order; replies go to sockets still
    /// open. `Model::upload`'s push (if any) goes out on the same mpsc channel before the
    /// reply, so the push is always seen first.
    pub fn release_held(&self) {
        let mut model = self.model();
        model.hold_uploads = false;
        for held in std::mem::take(&mut model.held) {
            let result = model.upload(&held.payload);
            if let Some((status, response)) = result.reply {
                let frame = json!([held.join_ref, held.reference, "sync:v2", "phx_reply",
                    {"status": status, "response": response}])
                .to_string();
                let _ = model.outbound[held.connection].send(frame);
            }
        }
    }

    pub fn reject_next_upload(&self, code: &str) {
        self.model().reject_next_upload = Some(code.to_string());
    }

    pub fn drop_next_push(&self) {
        self.model().drop_next_push = true;
    }

    /// `get_changes_since` below `seq` answers `cursor_too_old`.
    pub fn trim_log_through(&self, seq: i64) {
        self.model().trimmed_through = seq;
    }

    pub fn head(&self) -> i64 {
        self.model().head()
    }

    /// Another device writes `content` (creating the document if needed); pushed to every socket.
    pub fn put_doc(&self, doc_id: Uuid, content: Value) -> i64 {
        let mut model = self.model();
        match model.docs.get_mut(&doc_id) {
            Some(doc) => {
                doc.content = content;
                doc.deleted = false;
            }
            None => {
                model.docs.insert(
                    doc_id,
                    ServerDoc {
                        content,
                        seq: 0,
                        deleted: false,
                    },
                );
            }
        }
        model.commit(doc_id, false, None, true)
    }

    /// Another device deletes the document; pushed to every socket.
    pub fn delete_doc(&self, doc_id: Uuid) -> i64 {
        self.model().commit(doc_id, true, None, true)
    }

    /// Rounded content, seq and deleted flag, as a page would show them.
    pub fn doc(&self, doc_id: Uuid) -> Option<(Value, i64, bool)> {
        let model = self.model();
        model
            .docs
            .get(&doc_id)
            .map(|doc| (jsonb(&doc.content), doc.seq, doc.deleted))
    }

    pub fn commits_for(&self, doc_id: Uuid) -> usize {
        self.model()
            .log
            .iter()
            .filter(|entry| entry.doc_id == doc_id)
            .count()
    }

    pub fn frames(&self) -> Vec<Frame> {
        self.model().frames.clone()
    }

    /// The `api_key` of every join, in order.
    pub fn join_keys(&self) -> Vec<String> {
        self.frames()
            .into_iter()
            .filter(|frame| frame.event == "phx_join")
            .filter_map(|frame| frame.payload["api_key"].as_str().map(str::to_string))
            .collect()
    }

    pub fn uploads_for(&self, doc_id: Uuid) -> Vec<Value> {
        let id = doc_id.to_string();
        self.frames()
            .into_iter()
            .filter(|frame| frame.event == "upload" && frame.payload["doc_id"] == id.as_str())
            .map(|frame| frame.payload)
            .collect()
    }

    pub fn user_agents(&self) -> Vec<Option<String>> {
        self.model().user_agents.clone()
    }

    /// Closes every open connection from the server side, without a Close frame.
    pub fn drop_connections(&self) {
        self.kill.send_modify(|generation| *generation += 1);
    }
}

impl Model {
    fn new(user_id: Uuid) -> Model {
        Model {
            mode: Mode::Normal,
            user_id,
            join_error: None,
            clock_ahead: None,
            seq: 0,
            docs: HashMap::new(),
            log: Vec::new(),
            trimmed_through: 0,
            replies: HashMap::new(),
            hold_uploads: false,
            held: Vec::new(),
            lose_next_upload: false,
            drop_after_next_upload: false,
            reject_next_upload: None,
            drop_next_push: false,
            frames: Vec::new(),
            user_agents: Vec::new(),
            outbound: Vec::new(),
            joined: HashSet::new(),
        }
    }

    fn head(&self) -> i64 {
        self.log.last().map_or(0, |entry| entry.seq)
    }

    fn envelope(&self, doc_id: Uuid, content: &Value, seq: i64) -> Value {
        json!({
            "doc_id": doc_id,
            "owner_id": self.user_id,
            "author_id": null,
            "read_only": false,
            "source_doc_id": null,
            "derived_from": null,
            "title": content.get("title").and_then(Value::as_str),
            "content": content,
            "hash": content_hash(&jsonb(content)),
            "seq": seq,
        })
    }

    /// Pages, snapshots and fetches read the stored row: jsonb-rounded content.
    fn stored_envelope(&self, doc_id: Uuid) -> Option<Value> {
        let doc = self.docs.get(&doc_id).filter(|doc| !doc.deleted)?;
        Some(self.envelope(doc_id, &jsonb(&doc.content), doc.seq))
    }

    fn change(entry: &LogEntry, doc: Value) -> Value {
        json!({
            "scope": "own",
            "seq": entry.seq,
            "prev_seq": entry.prev_seq,
            "doc_id": entry.doc_id,
            "kind": if entry.deleted { "delete" } else { "upsert" },
            "doc": doc,
            "client_id": null,
            "upload_id": entry.upload_id,
        })
    }

    /// Records a commit of the document's current content and, when `push`, pushes it as
    /// stored (unrounded) to every joined socket.
    fn commit(&mut self, doc_id: Uuid, deleted: bool, upload_id: Option<Uuid>, push: bool) -> i64 {
        self.seq += 1;
        let seq = self.seq;
        let entry = LogEntry {
            seq,
            prev_seq: self.head(),
            doc_id,
            deleted,
            upload_id,
        };
        let content = {
            let doc = self
                .docs
                .get_mut(&doc_id)
                .expect("committed documents exist");
            doc.seq = seq;
            doc.deleted = deleted;
            doc.content.clone()
        };
        let pushed = if deleted {
            Value::Null
        } else {
            self.envelope(doc_id, &content, seq)
        };
        let change = Self::change(&entry, pushed);
        self.log.push(entry);
        if push && !std::mem::take(&mut self.drop_next_push) {
            let frame = json!([null, null, "sync:v2", "change", change]).to_string();
            for index in &self.joined {
                let _ = self.outbound[*index].send(frame.clone());
            }
        }
        seq
    }

    fn changes_since(&self, payload: &Value) -> (&'static str, Value) {
        let cursor = payload["cursor"].as_i64().unwrap_or(0);
        if payload["scope"] != "own" {
            return (
                "ok",
                json!({"changes": [], "next_cursor": cursor, "has_more": false}),
            );
        }
        if cursor < self.trimmed_through {
            return (
                "error",
                json!({"code": "cursor_too_old", "is_fatal": false}),
            );
        }
        let changes: Vec<Value> = self
            .log
            .iter()
            .filter(|entry| entry.seq > cursor)
            .filter_map(|entry| {
                if entry.deleted {
                    return Some(Self::change(entry, Value::Null));
                }
                self.stored_envelope(entry.doc_id)
                    .map(|doc| Self::change(entry, doc))
            })
            .collect();
        (
            "ok",
            json!({"changes": changes, "next_cursor": self.head().max(cursor), "has_more": false}),
        )
    }

    fn snapshot(&self, payload: &Value) -> (&'static str, Value) {
        if payload["scope"] != "own" {
            return (
                "ok",
                json!({"docs": [], "snapshot_seq": 0, "next_page_token": null}),
            );
        }
        let mut ids: Vec<Uuid> = self.docs.keys().copied().collect();
        ids.sort();
        let docs: Vec<Value> = ids
            .into_iter()
            .filter_map(|id| self.stored_envelope(id))
            .collect();
        (
            "ok",
            json!({"docs": docs, "snapshot_seq": self.head(), "next_page_token": null}),
        )
    }

    fn document(&self, payload: &Value) -> (&'static str, Value) {
        let doc_id = uuid_field(payload, "doc_id").unwrap_or_default();
        match self.docs.get(&doc_id) {
            None => ("error", json!({"code": "not_found", "is_fatal": false})),
            Some(doc) if doc.deleted => (
                "error",
                json!({"code": "deleted", "is_fatal": false, "current_seq": doc.seq}),
            ),
            Some(_) => ("ok", self.stored_envelope(doc_id).expect("a live document")),
        }
    }

    fn upload(&mut self, payload: &Value) -> UploadResult {
        let error = |code: &str| UploadResult {
            reply: Some(("error", json!({"code": code, "is_fatal": false}))),
            close: false,
        };
        let (Some(upload_id), Some(doc_id)) = (
            uuid_field(payload, "upload_id"),
            uuid_field(payload, "doc_id"),
        ) else {
            return error("validation");
        };
        let base_hash = payload
            .get("base_hash")
            .and_then(Value::as_str)
            .map(str::to_string);
        let key = (upload_id, base_hash.clone());
        if let Some(stored) = self.replies.get(&key) {
            return UploadResult {
                reply: Some(("ok", stored.clone())),
                close: false,
            };
        }
        if let Some(code) = self.reject_next_upload.take() {
            return error(&code);
        }
        let kind = payload["kind"].as_str().unwrap_or_default().to_string();
        let body = payload["payload"].clone();
        match (kind.as_str(), self.docs.get_mut(&doc_id)) {
            ("create", Some(_)) => {
                return UploadResult {
                    reply: Some((
                        "error",
                        json!({"code": "exists", "is_fatal": false, "existing_owner": self.user_id}),
                    )),
                    close: false,
                }
            }
            ("create", None) => {
                self.docs.insert(
                    doc_id,
                    ServerDoc {
                        content: body,
                        seq: 0,
                        deleted: false,
                    },
                );
            }
            ("update", Some(doc)) if !doc.deleted => {
                let current_hash = content_hash(&jsonb(&doc.content));
                if base_hash.as_deref() != Some(current_hash.as_str()) {
                    return hash_mismatch(current_hash, doc.seq);
                }
                let Ok(patch) = serde_json::from_value::<json_patch::Patch>(body) else {
                    return error("validation");
                };
                // The real server patches the row it loaded from Postgres: jsonb-rounded.
                // Untouched fields stay rounded; only the patch's own values are as sent.
                let mut rounded = jsonb(&doc.content);
                if json_patch::patch(&mut rounded, &patch).is_err() {
                    return error("validation");
                }
                doc.content = rounded;
            }
            ("delete", Some(doc)) if !doc.deleted => {
                let current_hash = content_hash(&jsonb(&doc.content));
                if base_hash.as_ref().is_some_and(|base| *base != current_hash) {
                    return hash_mismatch(current_hash, doc.seq);
                }
            }
            _ => return error("not_found"),
        }
        let lost = std::mem::take(&mut self.lose_next_upload);
        let dropped = std::mem::take(&mut self.drop_after_next_upload);
        let seq = self.commit(doc_id, kind == "delete", Some(upload_id), !lost && !dropped);
        let content = self.docs[&doc_id].content.clone();
        let reply = self.envelope(doc_id, &content, seq);
        self.replies.insert(key, reply.clone());
        UploadResult {
            reply: (!lost && !dropped).then_some(("ok", reply)),
            close: dropped,
        }
    }
}

/// What the real server reads back from Postgres. It writes through Jason, which prints a float
/// in the shorter of fixed and scientific notation, and jsonb keeps the printed scale: `1.2e3`
/// reads back as the integer 1200, `1234.0` stays a float, `-0.0` becomes `0.0`.
fn jsonb(value: &Value) -> Value {
    match value {
        Value::Number(number) if number.is_f64() => {
            let float = number.as_f64().expect("an f64 number");
            if float == 0.0 {
                json!(0.0)
            } else if float.fract() == 0.0 && jason_prints_scientific(float) {
                let mut read_back = value.clone();
                canonicalise_numbers(&mut read_back);
                read_back
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(jsonb).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, field)| (key.clone(), jsonb(field)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Jason prints the shortest round-trip digits; for an integral float the scientific form
/// (`1.0e3`, `1.2e3`) wins only when strictly shorter than the fixed one (`1000.0`).
fn jason_prints_scientific(float: f64) -> bool {
    let shortest = format!("{:e}", float.abs());
    let (mantissa, exponent) = shortest.split_once('e').expect("LowerExp has an exponent");
    let digits = mantissa.replace('.', "").len();
    let exponent: usize = exponent
        .parse()
        .expect("an integral float's exponent is not negative");
    let scientific = digits.max(2) + 2 + exponent.to_string().len();
    let fixed = exponent + 1 + 2;
    scientific < fixed
}

fn uuid_field(payload: &Value, name: &str) -> Option<Uuid> {
    payload.get(name)?.as_str()?.parse().ok()
}

fn hash_mismatch(current_hash: String, current_seq: i64) -> UploadResult {
    UploadResult {
        reply: Some((
            "error",
            json!({"code": "hash_mismatch", "is_fatal": false,
                "current_hash": current_hash, "current_seq": current_seq}),
        )),
        close: false,
    }
}

async fn accept_loop(
    listener: TcpListener,
    model: Arc<Mutex<Model>>,
    stats: Arc<Stats>,
    kill: watch::Receiver<u64>,
) {
    while let Ok((tcp, _)) = listener.accept().await {
        let mut user_agent = None;
        let accepted = tokio_tungstenite::accept_hdr_async(
            tcp,
            |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                user_agent = request
                    .headers()
                    .get("user-agent")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                Ok(response)
            },
        )
        .await;
        let Ok(ws) = accepted else { continue };
        stats.opened();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (index, mode) = {
            let mut model = model.lock().unwrap();
            model.outbound.push(outbound_tx);
            model.user_agents.push(user_agent);
            (model.outbound.len() - 1, model.mode)
        };
        if mode == Mode::DropAfterUpgrade {
            drop(ws);
            stats.closed();
            continue;
        }
        tokio::spawn(serve(
            ws,
            index,
            model.clone(),
            stats.clone(),
            kill.clone(),
            outbound_rx,
        ));
    }
}

async fn serve(
    ws: WebSocketStream<TcpStream>,
    index: usize,
    model: Arc<Mutex<Model>>,
    stats: Arc<Stats>,
    mut kill: watch::Receiver<u64>,
    mut outbound: mpsc::UnboundedReceiver<String>,
) {
    kill.mark_unchanged();
    let (mut sink, mut stream) = ws.split();
    loop {
        let outcome = tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => handle(&model, index, &text),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
            Some(frame) = outbound.recv() => Outcome {
                replies: vec![frame],
                close: false,
            },
            _ = kill.changed() => break,
        };
        let mut failed = false;
        for frame in outcome.replies {
            if sink.send(Message::Text(frame)).await.is_err() {
                failed = true;
                break;
            }
        }
        if failed || outcome.close {
            break;
        }
    }
    model.lock().unwrap().joined.remove(&index);
    stats.closed();
}

fn handle(model: &Mutex<Model>, index: usize, text: &str) -> Outcome {
    let Ok((join_ref, reference, topic, event, payload)) =
        serde_json::from_str::<(Value, Value, String, String, Value)>(text)
    else {
        return Outcome::default();
    };
    let mut model = model.lock().unwrap();
    model.frames.push(Frame {
        connection: index,
        event: event.clone(),
        payload: payload.clone(),
    });
    if model.mode == Mode::SilentAfterUpgrade {
        return Outcome::default();
    }
    let reply = |status: &str, response: Value| {
        json!([join_ref, reference, topic, "phx_reply",
            {"status": status, "response": response}])
        .to_string()
    };
    let mut outcome = Outcome::default();
    match event.as_str() {
        "heartbeat" => outcome.replies.push(reply("ok", json!({}))),
        "phx_join" => {
            let skewed = model.clock_ahead.and_then(|ahead| {
                let server_time = crate::store::now_unix() + ahead;
                let signed = payload["timestamp"].as_i64().unwrap_or_default();
                ((signed - server_time).abs() > 300).then_some(server_time)
            });
            match (skewed, model.join_error.clone()) {
                (Some(server_time), _) => outcome.replies.push(reply(
                    "error",
                    json!({"code": "clock_skew", "is_fatal": false, "server_time": server_time}),
                )),
                (None, Some(code)) => outcome
                    .replies
                    .push(reply("error", json!({"code": code, "is_fatal": true}))),
                (None, None) => {
                    model.joined.insert(index);
                    let joined = json!({"user_id": model.user_id, "protocol_version": 2});
                    outcome.replies.push(reply("ok", joined));
                }
            }
        }
        "get_changes_since" => {
            let (status, response) = model.changes_since(&payload);
            outcome.replies.push(reply(status, response));
        }
        "get_snapshot" => {
            let (status, response) = model.snapshot(&payload);
            outcome.replies.push(reply(status, response));
        }
        "get_document" => {
            let (status, response) = model.document(&payload);
            outcome.replies.push(reply(status, response));
        }
        "upload" if model.hold_uploads => model.held.push(Held {
            connection: index,
            join_ref: join_ref.clone(),
            reference: reference.clone(),
            payload,
        }),
        "upload" => {
            let result = model.upload(&payload);
            if let Some((status, response)) = result.reply {
                outcome.replies.push(reply(status, response));
            }
            outcome.close = result.close;
        }
        _ => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::engine::machine::{Input, Request, Response};
    use crate::engine::types::{Upload, UploadKind};
    use crate::transport::connection::{Connection, Received};
    use crate::transport::wire::JoinAuth;

    const ME: Uuid = Uuid::from_u128(0xA);
    const WAIT: Duration = Duration::from_secs(5);

    async fn received(connection: &mut Connection) -> Received {
        timeout(WAIT, async {
            loop {
                let event = connection.next_event().await;
                if let Some(received) = connection.receive(event) {
                    return received;
                }
            }
        })
        .await
        .expect("input within 5 s")
    }

    async fn nothing_within(connection: &mut Connection, wait: Duration) -> bool {
        timeout(wait, async {
            loop {
                let event = connection.next_event().await;
                if connection.receive(event).is_some() {
                    return;
                }
            }
        })
        .await
        .is_err()
    }

    /// A connection that has opened and joined as `ME` with `api_key`.
    async fn joined(server: &ScriptedServer, api_key: &str) -> Connection {
        let auth = JoinAuth {
            email: "a@b.c".into(),
            api_key: api_key.into(),
            api_secret: "rps_test".into(),
        };
        let mut connection = Connection::new(server.url.clone(), Some(auth), "ua/1".into());
        connection.open(1);
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketOpened { gen: 1 })
        );
        assert_eq!(connection.send(1, &Request::Join, 0), None);
        assert_eq!(
            received(&mut connection).await,
            Received::Joined {
                req: 1,
                user_id: ME
            }
        );
        connection
    }

    async fn reply(connection: &mut Connection, req: u64, request: Request) -> Received {
        assert_eq!(connection.send(req, &request, 0), None);
        received(connection).await
    }

    fn upload(
        upload_id: u128,
        doc_id: Uuid,
        kind: UploadKind,
        base: Option<String>,
        payload: Value,
    ) -> Request {
        Request::Upload(Upload {
            upload_id: Uuid::from_u128(upload_id),
            doc_id,
            kind,
            base_hash: base,
            payload,
        })
    }

    #[tokio::test]
    async fn joins_and_serves_an_empty_catch_up() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        assert_eq!(
            reply(
                &mut connection,
                2,
                Request::GetSnapshot {
                    scope: "own".into(),
                    page_token: None
                }
            )
            .await,
            Received::Input(Input::Reply {
                req: 2,
                result: Ok(Response::SnapshotPage {
                    docs: vec![],
                    snapshot_seq: 0,
                    next_page_token: None
                })
            })
        );
        assert_eq!(
            reply(
                &mut connection,
                3,
                Request::GetChangesSince {
                    scope: "own".into(),
                    cursor: 0,
                    limit: 500
                }
            )
            .await,
            Received::Input(Input::Reply {
                req: 3,
                result: Ok(Response::Changes {
                    changes: vec![],
                    next_cursor: 0,
                    has_more: false
                })
            })
        );
        assert_eq!(
            reply(&mut connection, 4, Request::Heartbeat).await,
            Received::Input(Input::Reply {
                req: 4,
                result: Ok(Response::HeartbeatOk)
            })
        );
        assert_eq!(server.user_agents(), vec![Some("ua/1".to_string())]);
        assert_eq!(server.join_keys(), vec!["k1".to_string()]);
        assert_eq!(server.stats.upgrades(), 1);
    }

    #[test]
    fn jsonb_matches_the_servers_postgres_round_trip() {
        // Measured 2026-09-29: Jason prints the float, Postgres jsonb stores that text's scale.
        let cases = [
            (json!(1200.0), json!(1200)),
            (json!(1000.0), json!(1000)),
            (json!(-1200.0), json!(-1200)),
            (json!(100000.0), json!(100000)),
            (json!(1.0e16), json!(10_000_000_000_000_000_i64)),
            (json!(1.0e19), json!(10_000_000_000_000_000_000_u64)),
            (json!(1.0e20), json!(1.0e20)),
            (json!(1234.0), json!(1234.0)),
            (json!(123456.0), json!(123456.0)),
            (json!(100.0), json!(100.0)),
            (json!(10.0), json!(10.0)),
            (json!(1.0), json!(1.0)),
            (json!(1.5), json!(1.5)),
            (json!(1200.5), json!(1200.5)),
            (json!(1.0e-5), json!(1.0e-5)),
            (json!(7), json!(7)),
        ];
        for (written, read_back) in cases {
            let got = jsonb(&written);
            assert_eq!(got, read_back, "{written}");
            assert_eq!(got.is_f64(), read_back.is_f64(), "{written}: number kind");
        }
        let zero = jsonb(&json!(-0.0));
        assert!(zero.is_f64() && !zero.as_f64().unwrap().is_sign_negative());
        for written in [
            json!(1200.0),
            json!(1234.0),
            json!(-0.0),
            json!(1.0e19),
            json!(1.0e20),
        ] {
            let mut canonical = written.clone();
            canonicalise_numbers(&mut canonical);
            assert_eq!(
                jsonb(&canonical),
                canonical,
                "{written}: canonical content survives"
            );
        }
    }

    #[tokio::test]
    async fn upload_is_applied_pushed_and_its_reply_stored() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        let doc_id = Uuid::from_u128(0xD1);
        let create = upload(0x71, doc_id, UploadKind::Create, None, json!({"n": 1200.0}));
        let Received::Input(Input::Reply {
            req: 2,
            result: Ok(Response::Uploaded(reply_doc)),
        }) = reply(&mut connection, 2, create.clone()).await
        else {
            panic!("expected the create to succeed");
        };
        assert_eq!(reply_doc.seq, 1);
        assert_eq!(
            reply_doc.content,
            json!({"n": 1200}),
            "the client decodes every envelope to canonical numbers"
        );
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected the create's push");
        };
        assert_eq!(change.upload_id, Some(Uuid::from_u128(0x71)));
        let Received::Input(Input::Reply {
            req: 3,
            result: Ok(Response::Uploaded(again)),
        }) = reply(&mut connection, 3, create).await
        else {
            panic!("expected the stored reply");
        };
        assert_eq!(again.seq, 1);
        assert_eq!(
            server.commits_for(doc_id),
            1,
            "a repeated upload commits nothing"
        );
        let Received::Input(Input::Reply {
            req: 4,
            result: Ok(Response::Document(fetched)),
        }) = reply(&mut connection, 4, Request::GetDocument { doc_id }).await
        else {
            panic!("expected the document");
        };
        assert_eq!(
            fetched.content,
            json!({"n": 1200}),
            "fetches carry jsonb-rounded content"
        );
        assert_eq!(fetched.hash, reply_doc.hash);
        let stale = upload(
            0x72,
            doc_id,
            UploadKind::Update,
            Some("stale".into()),
            json!([]),
        );
        let Received::Input(Input::Reply {
            req: 5,
            result: Err(error),
        }) = reply(&mut connection, 5, stale).await
        else {
            panic!("expected hash_mismatch");
        };
        assert_eq!(
            (error.code.as_str(), error.current_seq),
            ("hash_mismatch", Some(1))
        );
        let exists = upload(0x73, doc_id, UploadKind::Create, None, json!({}));
        let Received::Input(Input::Reply {
            req: 6,
            result: Err(error),
        }) = reply(&mut connection, 6, exists).await
        else {
            panic!("expected exists");
        };
        assert_eq!(
            (error.code.as_str(), error.existing_owner),
            ("exists", Some(ME))
        );
        assert_eq!(server.uploads_for(doc_id).len(), 4);
    }

    #[tokio::test]
    async fn delete_on_a_stale_base_is_a_hash_mismatch() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        let doc_id = Uuid::from_u128(0xD1);
        server.put_doc(doc_id, json!({"n": 1}));
        let _push = received(&mut connection).await;
        let stale = upload(
            0x71,
            doc_id,
            UploadKind::Delete,
            Some("stale".into()),
            Value::Null,
        );
        let Received::Input(Input::Reply {
            req: 2,
            result: Err(error),
        }) = reply(&mut connection, 2, stale).await
        else {
            panic!("expected hash_mismatch");
        };
        assert_eq!(
            (error.code.as_str(), error.current_seq),
            ("hash_mismatch", Some(1))
        );
        assert!(!server.doc(doc_id).unwrap().2, "nothing was deleted");
        let current = upload(
            0x72,
            doc_id,
            UploadKind::Delete,
            error.current_hash,
            Value::Null,
        );
        let Received::Input(Input::Reply {
            req: 3,
            result: Ok(Response::Uploaded(_)),
        }) = reply(&mut connection, 3, current).await
        else {
            panic!("expected the delete on the current base to land");
        };
        assert!(server.doc(doc_id).unwrap().2);
    }

    #[tokio::test]
    async fn update_patches_the_rounded_stored_content() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        let doc_id = Uuid::from_u128(0xD1);
        let create = upload(
            0x71,
            doc_id,
            UploadKind::Create,
            None,
            json!({"n": 1200.0, "m": 1}),
        );
        let Received::Input(Input::Reply {
            req: 2,
            result: Ok(Response::Uploaded(created)),
        }) = reply(&mut connection, 2, create).await
        else {
            panic!("expected the create to succeed");
        };
        let Received::Input(Input::Push(_)) = received(&mut connection).await else {
            panic!("expected the create's push");
        };
        let patch = json!([{"op": "replace", "path": "/m", "value": 2}]);
        let update = upload(
            0x72,
            doc_id,
            UploadKind::Update,
            Some(created.hash.clone()),
            patch,
        );
        let Received::Input(Input::Reply {
            req: 3,
            result: Ok(Response::Uploaded(updated)),
        }) = reply(&mut connection, 3, update).await
        else {
            panic!("expected the update to succeed");
        };
        assert_eq!(
            updated.content,
            json!({"n": 1200, "m": 2}),
            "an untouched field carries the jsonb-rounded stored form; only the patch's own \
             value is as sent, as the real server patches the row it loaded from Postgres"
        );
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected the update's push");
        };
        assert_eq!(
            change.doc.map(|doc| doc.content),
            Some(json!({"n": 1200, "m": 2}))
        );
    }

    #[tokio::test]
    async fn other_device_writes_are_pushed_paged_and_trimmed() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        let first = Uuid::from_u128(0xD1);
        let second = Uuid::from_u128(0xD2);
        assert_eq!(server.put_doc(first, json!({"a": 1})), 1);
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected a push");
        };
        assert_eq!((change.seq, change.prev_seq), (1, 0));
        server.drop_next_push();
        server.put_doc(second, json!({"b": 1}));
        assert_eq!(server.delete_doc(first), 3);
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected the delete's push");
        };
        assert_eq!((change.seq, change.prev_seq, change.doc), (3, 2, None));
        let Received::Input(Input::Reply {
            result:
                Ok(Response::Changes {
                    changes,
                    next_cursor,
                    ..
                }),
            ..
        }) = reply(
            &mut connection,
            2,
            Request::GetChangesSince {
                scope: "own".into(),
                cursor: 0,
                limit: 500,
            },
        )
        .await
        else {
            panic!("expected a page");
        };
        let seqs: Vec<i64> = changes.iter().map(|c| c.seq).collect();
        assert_eq!(
            seqs,
            vec![2, 3],
            "the deleted doc's upsert is covered by its delete"
        );
        assert_eq!(next_cursor, 3);
        assert_eq!(server.head(), 3);
        let Received::Input(Input::Reply {
            result: Err(error), ..
        }) = reply(&mut connection, 3, Request::GetDocument { doc_id: first }).await
        else {
            panic!("expected deleted");
        };
        assert_eq!(
            (error.code.as_str(), error.current_seq),
            ("deleted", Some(3))
        );
        server.trim_log_through(3);
        let Received::Input(Input::Reply {
            result: Err(error), ..
        }) = reply(
            &mut connection,
            4,
            Request::GetChangesSince {
                scope: "own".into(),
                cursor: 1,
                limit: 500,
            },
        )
        .await
        else {
            panic!("expected cursor_too_old");
        };
        assert_eq!(error.code, "cursor_too_old");
        assert_eq!(server.doc(second), Some((json!({"b": 1}), 2, false)));
    }

    #[tokio::test]
    async fn lost_held_rejected_and_dropped_uploads() {
        let server = ScriptedServer::start(ME).await;
        let mut connection = joined(&server, "k1").await;
        let lost = Uuid::from_u128(0xD1);
        server.lose_next_upload();
        assert_eq!(
            connection.send(
                2,
                &upload(0x71, lost, UploadKind::Create, None, json!({})),
                0
            ),
            None
        );
        assert!(nothing_within(&mut connection, Duration::from_millis(300)).await);
        assert_eq!(
            server.commits_for(lost),
            1,
            "a lost upload is still applied"
        );

        server.reject_next_upload("validation");
        let Received::Input(Input::Reply {
            result: Err(error), ..
        }) = reply(
            &mut connection,
            3,
            upload(
                0x72,
                Uuid::from_u128(0xD2),
                UploadKind::Create,
                None,
                json!({}),
            ),
        )
        .await
        else {
            panic!("expected validation");
        };
        assert_eq!(error.code, "validation");

        let held = Uuid::from_u128(0xD3);
        server.hold_uploads();
        assert_eq!(
            connection.send(
                4,
                &upload(0x73, held, UploadKind::Create, None, json!({})),
                0
            ),
            None
        );
        assert!(nothing_within(&mut connection, Duration::from_millis(300)).await);
        assert_eq!(server.commits_for(held), 0);
        server.release_held();
        // held uploads: push before reply — both go out on the same mpsc channel to this
        // connection, the push queued first inside `Model::upload`, so it always arrives first.
        let Received::Input(Input::Push(change)) = received(&mut connection).await else {
            panic!("expected the held upload's push before its reply");
        };
        assert_eq!(change.doc_id, held);
        let Received::Input(Input::Reply {
            req: 4,
            result: Ok(Response::Uploaded(_)),
        }) = received(&mut connection).await
        else {
            panic!("expected the held upload's reply after its push");
        };
        assert_eq!(server.commits_for(held), 1);

        server.drop_after_next_upload();
        assert_eq!(
            connection.send(
                5,
                &upload(
                    0x74,
                    Uuid::from_u128(0xD4),
                    UploadKind::Create,
                    None,
                    json!({})
                ),
                0
            ),
            None
        );
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketClosed { gen: 1 }),
            "the connection closes after the upload is applied, with no reply and no push"
        );
    }

    #[tokio::test]
    async fn modes_and_join_rejection() {
        let server = ScriptedServer::start(ME).await;
        server.reject_join("auth_invalid");
        let auth = JoinAuth {
            email: "a@b.c".into(),
            api_key: "k1".into(),
            api_secret: "rps_test".into(),
        };
        let mut connection = Connection::new(server.url.clone(), Some(auth), "ua/1".into());
        connection.open(1);
        received(&mut connection).await;
        let Received::Input(Input::Reply {
            result: Err(error), ..
        }) = reply(&mut connection, 1, Request::Join).await
        else {
            panic!("expected the join to be refused");
        };
        assert_eq!(
            (error.code.as_str(), error.is_fatal),
            ("auth_invalid", true)
        );
        server.accept_joins();
        connection.close(1);
        no_live_connections(&server).await;

        server.set_mode(Mode::DropAfterUpgrade);
        connection.open(2);
        let mut outcome = Vec::new();
        for _ in 0..2 {
            outcome.push(received(&mut connection).await);
        }
        assert_eq!(
            outcome.last(),
            Some(&Received::Input(Input::SocketClosed { gen: 2 }))
        );

        server.set_mode(Mode::SilentAfterUpgrade);
        connection.open(3);
        received(&mut connection).await;
        assert_eq!(connection.send(1, &Request::Join, 0), None);
        assert!(nothing_within(&mut connection, Duration::from_millis(300)).await);
        server.drop_connections();
        assert_eq!(
            received(&mut connection).await,
            Received::Input(Input::SocketClosed { gen: 3 })
        );
        assert_eq!(server.stats.upgrades(), 3);
        no_live_connections(&server).await;
        assert_eq!(server.stats.peak_live(), 1);

        let connections: Vec<usize> = server
            .frames()
            .iter()
            .map(|frame| frame.connection)
            .collect();
        assert!(
            connections.contains(&2),
            "frames record the connection index that sent them"
        );

        // A join after drop_connections() still succeeds.
        server.set_mode(Mode::Normal);
        let mut after_drop = joined(&server, "k2").await;
        after_drop.close(1);
        no_live_connections(&server).await;
    }

    async fn no_live_connections(server: &ScriptedServer) {
        timeout(WAIT, async {
            while server.stats.live() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("every connection ends");
    }
}

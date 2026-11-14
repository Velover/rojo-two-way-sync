//! Everything the sync serves, independent of any web framework: the API the plugin talks to, the
//! review page and its API, the file watcher, and opening the browser or VS Code.
//!
//! A host (the Rojo fork) mounts it somewhere, strips that prefix, and passes
//! each request to [`Service::handle`]. Paths here are relative to the mount: `/` is the review page
//! and `/api/...` the API. The page uses relative URLs, so it works under any prefix.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use notify::{RecursiveMode, Watcher};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::config::Config;
use crate::engine::{Engine, StudioEntry};
use crate::modeldiff::{self, Status};
use crate::tree::Kind;

pub const REVIEW_PAGE: &str = include_str!("review.html");
const POLL_SECONDS: u64 = 15;
const SESSION_TIMEOUT_SECONDS: u64 = 45;
const TICK: Duration = Duration::from_secs(2);
const PAGE_ALIVE: Duration = Duration::from_secs(5);

/// One request, as the host received it, with the mount prefix already removed from `path`.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

/// What to send back. The host turns it into its own response type.
pub enum Reply {
    Json(u16, Value),
    Bytes(Vec<u8>),
    Html(&'static str),
}

impl Reply {
    fn ok() -> Self {
        Reply::Json(200, json!({ "ok": true }))
    }

    fn gone() -> Self {
        Reply::Json(410, json!({ "error": "session ended" }))
    }
}

pub struct Service {
    engine: Mutex<Engine>,
    wake: Notify,
    /// The review page's full URL, for opening it in the browser.
    page_url: String,
    open_browser: bool,
    page_seen: Mutex<Option<Instant>>,
}

#[derive(Deserialize)]
struct SessionQuery {
    #[serde(default)]
    session: String,
    #[serde(default)]
    mapping: usize,
    #[serde(default)]
    key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectBody {
    place_id: u64,
    place_name: String,
}

#[derive(Deserialize)]
struct SnapshotBody {
    entries: Vec<StudioEntry>,
}

#[derive(Deserialize)]
struct ChangesBody {
    #[serde(default)]
    upserts: Vec<StudioEntry>,
    #[serde(default)]
    deletes: Vec<String>,
}

#[derive(Deserialize)]
struct AckBody {
    op: u64,
    ok: bool,
    #[serde(default)]
    kept: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveBody {
    choice: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    studio_hash: Option<String>,
    #[serde(default)]
    disk_hash: Option<String>,
}

fn json_body<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(body)?)
}

impl Service {
    pub fn new(
        config: Config,
        root: PathBuf,
        page_url: String,
        open_browser: bool,
    ) -> Result<Arc<Self>> {
        let engine = Engine::new(config, root)?;
        let tidied = engine.tidy()?;
        if tidied.blobs > 0 || tidied.history_lines > 0 {
            println!(
                "twoway: removed {} stored version(s) and {} history line(s) nothing needs any more",
                tidied.blobs, tidied.history_lines
            );
        }
        Ok(Arc::new(Self {
            engine: Mutex::new(engine),
            wake: Notify::new(),
            page_url,
            open_browser,
            page_seen: Mutex::new(None),
        }))
    }

    pub fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs the engine, then wakes the plugin and, for new conflicts, opens the review page.
    fn settle(&self, mut engine: MutexGuard<'_, Engine>) -> Result<()> {
        let new_conflicts = engine.reconcile()?;
        drop(engine);
        self.wake.notify_waiters();
        if new_conflicts > 0 {
            println!("{new_conflicts} conflict(s) to review: {}", self.page_url);
            self.open_review();
        }
        Ok(())
    }

    fn open_review(&self) {
        let seen = *self.page_seen.lock().unwrap_or_else(|e| e.into_inner());
        if !self.open_browser || seen.is_some_and(|at| at.elapsed() < PAGE_ALIVE) {
            return;
        }
        if let Err(error) = open_in_browser(&self.page_url) {
            eprintln!("warning: could not open the review page: {error}");
        }
    }

    /// Answers one request. Errors become a 400 with `{ "error": ... }`.
    pub async fn handle(&self, request: Request) -> Reply {
        match self.route(&request).await {
            Ok(reply) => reply,
            Err(error) => Reply::Json(400, json!({ "error": format!("{error:#}") })),
        }
    }

    async fn route(&self, request: &Request) -> Result<Reply> {
        let query: SessionQuery = serde_urlencoded::from_str(&request.query)?;
        let parts: Vec<&str> = request.path.trim_matches('/').split('/').collect();
        let body = &request.body;
        Ok(match (request.method.as_str(), parts.as_slice()) {
            ("GET", [""]) => Reply::Html(REVIEW_PAGE),
            ("POST", ["api", "connect"]) => self.connect(json_body(body)?)?,
            ("POST", ["api", "disconnect"]) => self.disconnect(&query),
            ("POST", ["api", "model"]) => self.upload_model(&query, body)?,
            ("POST", ["api", "snapshot"]) => self.snapshot(&query, json_body(body)?)?,
            ("POST", ["api", "changes"]) => self.changes(&query, json_body(body)?)?,
            ("GET", ["api", "poll"]) => self.poll(&query).await,
            ("POST", ["api", "ack"]) => self.ack(&query, json_body(body)?)?,
            ("GET", ["api", "blob", hash]) => Reply::Bytes(self.engine().store.get(hash)?),
            ("GET", ["api", "review"]) => self.review_state(),
            ("GET", ["api", "status"]) => self.status(),
            ("POST", ["api", "open-review"]) => {
                open_in_browser(&self.page_url)?;
                Reply::ok()
            }
            // For `rojo twoway conflicts`. Unlike `review`, this doesn't count as the page being open.
            ("GET", ["api", "conflicts"]) => {
                Reply::Json(200, json!({ "conflicts": self.engine().conflicts() }))
            }
            ("GET", ["api", "conflicts", id]) => self.conflict_detail(id.parse()?)?,
            ("POST", ["api", "conflicts", id, "resolve"]) => {
                self.resolve(id.parse()?, json_body(body)?)?
            }
            ("POST", ["api", "conflicts", id, "open-diff"]) => self.open_diff(id.parse()?)?,
            ("POST", ["api", "conflicts", id, "open-merge"]) => self.open_merge(id.parse()?)?,
            _ => Reply::Json(
                404,
                json!({ "error": format!("no route {} {}", request.method, request.path) }),
            ),
        })
    }

    fn connect(&self, body: ConnectBody) -> Result<Reply> {
        let mut engine = self.engine();
        let connected = engine.connect(body.place_id, &body.place_name)?;
        println!("connected: {} ({})", body.place_name, body.place_id);
        Ok(Reply::Json(200, serde_json::to_value(connected)?))
    }

    fn disconnect(&self, query: &SessionQuery) -> Reply {
        let mut engine = self.engine();
        if engine.is_session(&query.session) {
            engine.disconnect();
            println!("disconnected");
        }
        drop(engine);
        self.wake.notify_waiters();
        Reply::ok()
    }

    /// The engine, if this request belongs to the current session.
    fn session_engine(&self, query: &SessionQuery) -> Option<MutexGuard<'_, Engine>> {
        let mut engine = self.engine();
        if !engine.is_session(&query.session) {
            return None;
        }
        engine.touch();
        Some(engine)
    }

    fn upload_model(&self, query: &SessionQuery, body: &[u8]) -> Result<Reply> {
        let Some(mut engine) = self.session_engine(query) else {
            return Ok(Reply::gone());
        };
        engine.upload_model(query.mapping, &query.key, body)?;
        self.settle(engine)?;
        Ok(Reply::ok())
    }

    fn snapshot(&self, query: &SessionQuery, body: SnapshotBody) -> Result<Reply> {
        let Some(mut engine) = self.session_engine(query) else {
            return Ok(Reply::gone());
        };
        engine.snapshot(query.mapping, body.entries)?;
        self.settle(engine)?;
        Ok(Reply::ok())
    }

    fn changes(&self, query: &SessionQuery, body: ChangesBody) -> Result<Reply> {
        let Some(mut engine) = self.session_engine(query) else {
            return Ok(Reply::gone());
        };
        engine.changes(query.mapping, body.upserts, body.deletes)?;
        self.settle(engine)?;
        Ok(Reply::ok())
    }

    /// Waits up to `POLL_SECONDS` for ops.
    async fn poll(&self, query: &SessionQuery) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(POLL_SECONDS);
        loop {
            let waiter = self.wake.notified();
            {
                let Some(mut engine) = self.session_engine(query) else {
                    return Reply::gone();
                };
                let ops = engine.take_ops();
                if !ops.is_empty() {
                    return Reply::Json(200, json!({ "ops": ops }));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, waiter).await.is_err() {
                return Reply::Json(200, json!({ "ops": [] }));
            }
        }
    }

    fn ack(&self, query: &SessionQuery, body: AckBody) -> Result<Reply> {
        let mut engine = self.engine();
        if !engine.is_session(&query.session) {
            return Ok(Reply::gone());
        }
        engine.ack(body.op, body.ok, body.kept)?;
        self.settle(engine)?;
        Ok(Reply::ok())
    }

    /// For the **Two-way sync** box in Rojo's plugin. Unlike `review_state`, this doesn't count as the page being open.
    fn status(&self) -> Reply {
        let engine = self.engine();
        Reply::Json(
            200,
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "connectedPlace": engine.place_name(),
                "conflicts": engine.conflicts().len(),
                "warnings": engine.warnings,
            }),
        )
    }

    fn review_state(&self) -> Reply {
        *self.page_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let engine = self.engine();
        Reply::Json(
            200,
            json!({
                "connected": engine.has_session(),
                "place": engine.place_name(),
                "conflicts": engine.conflicts(),
                "warnings": engine.warnings,
                "history": engine.store.history(30),
            }),
        )
    }

    /// One conflict in full: texts and diff for a script, an instance tree for a model.
    pub fn conflict_detail_value(&self, id: u64) -> Result<Value> {
        let engine = self.engine();
        let conflict = engine
            .conflicts()
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| anyhow!("no such conflict"))?;
        let is_model = [&conflict.studio, &conflict.disk]
            .iter()
            .any(|side| side.as_ref().is_some_and(|i| i.kind == Kind::Model));

        if is_model {
            let (studio, disk) = engine.conflict_models(id)?;
            let nodes = match (&studio, &disk) {
                (Some(a), Some(b)) => modeldiff::diff(a, b)?,
                (Some(a), None) => modeldiff::outline(a, Status::OnlyStudio)?,
                (None, Some(b)) => modeldiff::outline(b, Status::OnlyDisk)?,
                (None, None) => Vec::new(),
            };
            return Ok(json!({ "model": nodes }));
        }

        let texts = engine.conflict_texts(id)?;
        let diff = match (&texts.disk, &texts.studio) {
            (Some(disk), Some(studio)) => Some(
                diffy::DiffOptions::new()
                    .set_original_filename("disk")
                    .set_modified_filename("studio")
                    .create_patch(disk, studio)
                    .to_string(),
            ),
            _ => None,
        };
        Ok(json!({ "texts": texts, "diff": diff }))
    }

    fn conflict_detail(&self, id: u64) -> Result<Reply> {
        Ok(Reply::Json(200, self.conflict_detail_value(id)?))
    }

    fn resolve(&self, id: u64, body: ResolveBody) -> Result<Reply> {
        let mut engine = self.engine();
        let seen = (body.studio_hash.as_deref(), body.disk_hash.as_deref());
        if body.choice == "mergeFile" {
            let dir = engine.store.temp_dir(&id.to_string())?;
            let text = std::fs::read_to_string(dir.join("merged.luau"))?;
            if text.contains("<<<<<<<") || text.contains(">>>>>>>") {
                bail!("the merge still has conflict markers");
            }
            engine.resolve(id, "merged", Some(&text), seen)?;
        } else {
            engine.resolve(id, &body.choice, body.text.as_deref(), seen)?;
        }
        engine.store.remove_temp(&id.to_string());
        drop(engine);
        self.wake.notify_waiters();
        Ok(Reply::ok())
    }

    fn open_diff(&self, id: u64) -> Result<Reply> {
        let engine = self.engine();
        let (studio, disk, ..) = write_sides(&engine, id)?;
        code(&[Path::new("--diff"), &disk, &studio])?;
        Ok(Reply::ok())
    }

    fn open_merge(&self, id: u64) -> Result<Reply> {
        let engine = self.engine();
        let (studio, disk, base, result, start) = write_sides(&engine, id)?;
        std::fs::write(&result, start)?;
        code(&[Path::new("--merge"), &studio, &disk, &base, &result])?;
        Ok(Reply::ok())
    }

    /// Reconciles shortly after files change, and on a slow tick as a safety net. The tick also
    /// picks up mapped folders that didn't exist when the service started. Runs until dropped.
    pub async fn watch(self: Arc<Self>) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        });
        let mut watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                eprintln!("warning: no file watcher, relying on the tick: {error}");
                None
            }
        };
        let mut watching: BTreeSet<PathBuf> = BTreeSet::new();
        let mut ticker = tokio::time::interval(TICK);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Some(watcher) = watcher.as_mut() {
                        for dir in self.engine().watched_dirs() {
                            if dir.is_dir() && !watching.contains(&dir) && watcher.watch(&dir, RecursiveMode::Recursive).is_ok() {
                                watching.insert(dir);
                            }
                        }
                    }
                }
                Some(_) = receiver.recv() => {
                    // Let a burst of events finish.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    while receiver.try_recv().is_ok() {}
                }
            }
            self.reconcile_now();
        }
    }

    fn reconcile_now(&self) {
        let mut engine = self.engine();
        if engine
            .seconds_since_seen()
            .is_some_and(|s| s > SESSION_TIMEOUT_SECONDS)
        {
            println!("the plugin went quiet, ending the session");
            engine.disconnect();
        }
        if engine.has_session() {
            if let Err(error) = self.settle(engine) {
                eprintln!("error: {error:#}");
            }
        }
    }
}

/// Opens a page in the default browser.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // `start` reads its first quoted argument as a window title, hence the empty one.
        Command::new("cmd").args(["/c", "start", "", url]).spawn()?;
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

fn code(args: &[&Path]) -> Result<()> {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/c", "code"]);
        command
    } else {
        Command::new("code")
    };
    command.args(args).spawn()?;
    Ok(())
}

fn write_sides(engine: &Engine, id: u64) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf, String)> {
    let texts = engine.conflict_texts(id)?;
    let dir = engine.store.temp_dir(&id.to_string())?;
    let (studio, disk, base) = (
        dir.join("studio.luau"),
        dir.join("disk.luau"),
        dir.join("base.luau"),
    );
    std::fs::write(&studio, texts.studio.as_deref().unwrap_or(""))?;
    std::fs::write(&disk, texts.disk.as_deref().unwrap_or(""))?;
    std::fs::write(&base, texts.base.as_deref().unwrap_or(""))?;
    let start = texts
        .merged
        .or(texts.merged_with_markers)
        .or(texts.disk)
        .unwrap_or_default();
    Ok((studio, disk, base, dir.join("merged.luau"), start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, Arc<Service>) {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_str(
            r#"{ "mappings": [ { "studio": "ReplicatedStorage/Kit", "path": "files" } ] }"#,
        )
        .unwrap();
        let service =
            Service::new(config, dir.path().to_path_buf(), "http://x/".into(), false).unwrap();
        (dir, service)
    }

    fn request(method: &str, path: &str, query: &str, body: Value) -> Request {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            body,
        }
    }

    fn json_of(reply: Reply) -> (u16, Value) {
        match reply {
            Reply::Json(status, value) => (status, value),
            Reply::Bytes(_) => (200, Value::Null),
            Reply::Html(_) => (200, Value::String("html".into())),
        }
    }

    #[tokio::test]
    async fn routes_cover_a_session() {
        let (_dir, service) = service();
        assert_eq!(
            json_of(service.handle(request("GET", "/", "", Value::Null)).await).1,
            "html"
        );

        let (status, connected) = json_of(
            service
                .handle(request(
                    "POST",
                    "/api/connect",
                    "",
                    json!({ "placeId": 0, "placeName": "x" }),
                ))
                .await,
        );
        assert_eq!(status, 200, "{connected}");
        let session = connected["session"].as_str().unwrap().to_string();
        let query = format!("session={session}&mapping=0");
        let entries = json!({ "entries": [ { "key": "", "kind": "folder" }, { "key": "A", "kind": "script", "scriptType": "module", "source": "return 1" } ] });
        let (status, _) = json_of(
            service
                .handle(request("POST", "/api/snapshot", &query, entries))
                .await,
        );
        assert_eq!(status, 200);
        assert_eq!(
            std::fs::read_to_string(_dir.path().join("files/A.luau")).unwrap(),
            "return 1"
        );

        let (status, _) = json_of(
            service
                .handle(request(
                    "POST",
                    "/api/changes",
                    "session=old&mapping=0",
                    json!({}),
                ))
                .await,
        );
        assert_eq!(status, 410, "another session's request is refused");
        let (status, _) = json_of(
            service
                .handle(request("GET", "/api/nothing", "", Value::Null))
                .await,
        );
        assert_eq!(status, 404);
        let (status, error) = json_of(
            service
                .handle(request("POST", "/api/connect", "", json!({ "bad": 1 })))
                .await,
        );
        assert_eq!(status, 400, "{error}");
    }

    /// Opens a real browser tab, so it only runs when asked: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn opens_the_default_browser() {
        open_in_browser("http://127.0.0.1:34910/status").unwrap();
    }
}

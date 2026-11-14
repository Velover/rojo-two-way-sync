//! The sync engine.
//!
//! Three versions of every item are compared: the base (what both sides last agreed on), Studio
//! and disk. `reconcile` looks at all three and works out what to do, so it can run after any
//! event, any number of times. Nothing depends on timing: our own writes come back as "both sides
//! match", which needs no action.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use globset::GlobSet;
use serde::{Deserialize, Serialize};

use crate::config::{Config, Mapping};
use crate::modelfile::{self, ModelFormat};
use crate::tree::{
    canonical_meta, default_path, depth, hash_bytes, is_group_key, key_dir, meta_path, model_path,
    name_problem, normalize_text, parent_key, prune_folders, scan, split_meta, write_meta,
    DiskTree, Item, Kind, ModelRule, ScanOptions, ScriptType, Tree,
};

/// Base copies, every version seen, and the history, in `.twoway/` beside the config.
pub struct Store {
    dir: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    bases: BTreeMap<String, Tree>,
    /// Versions both sides once agreed on, by mapping and key, oldest first. A place reopened
    /// without saving shows up with one of these.
    #[serde(default)]
    past: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

const PAST_VERSIONS_KEPT: usize = 100;

impl State {
    fn remember_base(&mut self, mapping: &str) {
        let Some(base) = self.bases.get(mapping) else {
            return;
        };
        let past = self.past.entry(mapping.to_string()).or_default();
        for (key, item) in base {
            let Some(hash) = &item.hash else { continue };
            let versions = past.entry(key.clone()).or_default();
            if versions.last() != Some(hash) {
                versions.retain(|v| v != hash);
                versions.push(hash.clone());
                if versions.len() > PAST_VERSIONS_KEPT {
                    versions.remove(0);
                }
            }
        }
    }

    fn is_past_version(&self, mapping: &str, key: &str, item: &Option<Item>) -> bool {
        let Some(hash) = item.as_ref().and_then(|i| i.hash.as_ref()) else {
            return false;
        };
        self.past
            .get(mapping)
            .and_then(|p| p.get(key))
            .is_some_and(|v| v.contains(hash))
    }
}

impl Store {
    pub fn open(project_root: &Path) -> Result<Self> {
        let dir = project_root.join(".twoway");
        fs::create_dir_all(dir.join("blobs"))?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n")?;
        }
        Ok(Self { dir })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = hash_bytes(bytes);
        let path = self.dir.join("blobs").join(&hash);
        if !path.exists() {
            fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("bad hash");
        }
        fs::read(self.dir.join("blobs").join(hash)).with_context(|| format!("no blob {hash}"))
    }

    fn load_state(&self) -> Result<State> {
        match fs::read_to_string(self.dir.join("state.json")) {
            Ok(text) => Ok(serde_json::from_str(&text).context(".twoway/state.json is not valid")?),
            Err(_) => Ok(State::default()),
        }
    }

    fn save_state(&self, state: &State) -> Result<()> {
        let temp = self.dir.join("state.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(temp, self.dir.join("state.json"))?;
        Ok(())
    }

    fn log(&self, entry: &HistoryEntry) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("history.jsonl"))?;
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
        Ok(())
    }

    pub fn history(&self, limit: usize) -> Vec<HistoryEntry> {
        let text = fs::read_to_string(self.dir.join("history.jsonl")).unwrap_or_default();
        let mut entries: Vec<HistoryEntry> = text
            .lines()
            .rev()
            .take(limit)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        entries.reverse();
        entries
    }

    pub fn temp_dir(&self, name: &str) -> Result<PathBuf> {
        let dir = self.dir.join("tmp").join(name);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub at: u64,
    pub mapping: String,
    pub key: String,
    pub direction: String,
    pub change: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// One item as Studio reports it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioEntry {
    pub key: String,
    pub kind: Kind,
    #[serde(default)]
    pub script_type: Option<ScriptType>,
    #[serde(default)]
    pub source: Option<String>,
    /// Encoded as in a Rojo meta file. Luau sends an empty table as `[]`.
    #[serde(default)]
    pub attributes: Option<serde_json::Value>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// A folder's loose instances, together under the key `<folder>/init`.
    #[serde(default)]
    pub group: bool,
}

/// An instruction for the plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Op {
    pub id: u64,
    pub mapping: usize,
    pub key: String,
    /// `upsert` or `delete`.
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_type: Option<ScriptType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_context: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// For a model: fetch the bytes from `/api/blob/<hash>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// For a folder or script: the whole set, so anything missing is removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub id: u64,
    pub mapping: usize,
    pub studio_root: String,
    pub key: String,
    pub reason: String,
    /// The side to offer first, when one is clearly right.
    pub suggested: Option<&'static str>,
    pub base: Option<Item>,
    pub studio: Option<Item>,
    pub disk: Option<Item>,
    pub disk_path: Option<PathBuf>,
}

/// Each side's model bytes in a conflict, Studio's first.
pub type ModelSides = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The texts behind a conflict, for the review page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictTexts {
    pub base: Option<String>,
    pub studio: Option<String>,
    pub disk: Option<String>,
    /// A clean three-way merge, when one exists.
    pub merged: Option<String>,
    /// The merge with conflict markers, when the edits overlap.
    pub merged_with_markers: Option<String>,
    /// Attributes and tags of each side, readable, when they differ.
    pub studio_meta: Option<String>,
    pub disk_meta: Option<String>,
}

struct Live {
    mapping: Mapping,
    id: String,
    root: PathBuf,
    models: ModelRule,
    studio: Option<Tree>,
    /// The latest bytes for each model, by key, uploaded before the snapshot that lists it.
    uploaded_models: BTreeMap<String, String>,
    /// True until the first reconcile of this session, when the two sides may have drifted apart
    /// while nothing was connected. Deletions found then always go to review.
    fresh: bool,
}

struct InFlight {
    mapping: usize,
    key: String,
    target: Option<Item>,
}

struct Session {
    id: String,
    place_name: String,
    live: Vec<Live>,
    ops: VecDeque<Op>,
    in_flight: BTreeMap<u64, InFlight>,
    last_seen: Instant,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingInfo {
    pub index: usize,
    pub studio: String,
    pub models: Vec<String>,
    /// Every instance that isn't a script or a Folder is a model.
    pub auto: bool,
    pub exclude: Vec<String>,
    /// In auto mode, a folder's loose instances share one file, except under `separate`.
    pub group: bool,
    pub separate: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connected {
    pub session: String,
    pub mappings: Vec<MappingInfo>,
}

pub struct Engine {
    pub config: Config,
    root: PathBuf,
    pub store: Store,
    ignore: GlobSet,
    state: State,
    session: Option<Session>,
    conflicts: Vec<Conflict>,
    next_id: u64,
    pub warnings: Vec<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn is_init_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(crate::tree::parse_script_file)
        .is_some_and(|(name, _)| name == "init")
}

impl Engine {
    pub fn new(config: Config, root: PathBuf) -> Result<Self> {
        let store = Store::open(&root)?;
        let state = store.load_state()?;
        let ignore = config.ignore_set()?;
        Ok(Self {
            config,
            root,
            store,
            ignore,
            state,
            session: None,
            conflicts: Vec::new(),
            next_id: 1,
            warnings: Vec::new(),
        })
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn warn(&mut self, message: String) {
        if !self.warnings.contains(&message) {
            eprintln!("warning: {message}");
            self.warnings.push(message);
        }
    }

    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    pub fn place_name(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.place_name.as_str())
    }

    pub fn is_session(&self, id: &str) -> bool {
        self.session.as_ref().is_some_and(|s| s.id == id)
    }

    pub fn touch(&mut self) {
        if let Some(session) = &mut self.session {
            session.last_seen = Instant::now();
        }
    }

    pub fn seconds_since_seen(&self) -> Option<u64> {
        self.session
            .as_ref()
            .map(|s| s.last_seen.elapsed().as_secs())
    }

    pub fn connect(&mut self, place_id: u64, place_name: &str) -> Result<Connected> {
        let mappings = self.config.mappings_for(place_id, place_name);
        if mappings.is_empty() {
            bail!("no mapping applies to {place_name} ({place_id})");
        }
        let session_id = format!("{:x}", now_ms());
        let mut live: Vec<Live> = Vec::new();
        for mapping in mappings {
            live.push(Live {
                id: mapping.id(),
                root: self.root.join(&mapping.path),
                models: ModelRule::for_mapping(&mapping)?,
                mapping,
                studio: None,
                uploaded_models: BTreeMap::new(),
                fresh: true,
            });
        }
        let info = live
            .iter()
            .enumerate()
            .map(|(index, l)| MappingInfo {
                index,
                studio: l.mapping.studio.clone(),
                models: l.mapping.models.clone(),
                auto: l.models.auto,
                exclude: l.mapping.exclude.clone(),
                group: l.models.group,
                separate: l.mapping.separate.clone(),
            })
            .collect();
        self.conflicts.clear();
        self.session = Some(Session {
            id: session_id.clone(),
            place_name: place_name.to_string(),
            live,
            ops: VecDeque::new(),
            in_flight: BTreeMap::new(),
            last_seen: Instant::now(),
        });
        Ok(Connected {
            session: session_id,
            mappings: info,
        })
    }

    pub fn disconnect(&mut self) {
        self.session = None;
        self.conflicts.clear();
    }

    fn live_mut(&mut self, mapping: usize) -> Result<&mut Live> {
        self.session
            .as_mut()
            .and_then(|s| s.live.get_mut(mapping))
            .ok_or_else(|| anyhow!("no such mapping in this session"))
    }

    /// Studio's bytes for a model, always binary. They're kept in the mapping's format, so they
    /// compare with the file as it is written.
    pub fn upload_model(&mut self, mapping: usize, key: &str, bytes: &[u8]) -> Result<()> {
        let rules = self.live_mut(mapping)?.models.clone();
        if !rules.takes_file(key) {
            bail!("{key} is not a model in this mapping");
        }
        let converted = modelfile::convert(bytes, rules.format)?;
        let hash = self.store.put(&converted)?;
        let live = self.live_mut(mapping)?;
        live.uploaded_models.insert(key.to_string(), hash.clone());
        if let Some(studio) = &mut live.studio {
            studio.insert(
                key.to_string(),
                Item {
                    kind: Kind::Model,
                    script_type: None,
                    hash: Some(hash),
                    meta: None,
                },
            );
        }
        Ok(())
    }

    /// Turns what Studio sent into items, storing the text. Names that can't be files are dropped
    /// with a warning, along with anything under them.
    fn items_from_entries(&mut self, mapping: usize, entries: Vec<StudioEntry>) -> Result<Tree> {
        let mut tree = Tree::new();
        let mut bad: Vec<String> = Vec::new();
        let mut lowered: BTreeMap<String, String> = BTreeMap::new();

        let mut sorted = entries;
        sorted.sort_by_key(|e| depth(&e.key));
        for entry in sorted {
            if bad.iter().any(|b| entry.key.starts_with(&format!("{b}/"))) {
                continue;
            }
            let is_group = entry.group && entry.kind == Kind::Model && is_group_key(&entry.key);
            if let Some(name) = (!entry.key.is_empty() && !is_group)
                .then(|| entry.key.rsplit('/').next().unwrap_or(""))
            {
                if let Some(problem) = name_problem(name) {
                    self.warn(format!("skipped {}: {problem}", entry.key));
                    bad.push(entry.key.clone());
                    continue;
                }
            }
            if let Some(other) = lowered.insert(entry.key.to_lowercase(), entry.key.clone()) {
                if other != entry.key {
                    self.warn(format!(
                        "skipped {}: the same file name as {other}, apart from case",
                        entry.key
                    ));
                    bad.push(entry.key.clone());
                    continue;
                }
            }
            let meta = canonical_meta(entry.attributes.as_ref(), entry.tags.as_deref());
            let item = match entry.kind {
                Kind::Folder => Item {
                    meta,
                    ..Item::folder()
                },
                Kind::Script => {
                    let text = normalize_text(entry.source.as_deref().unwrap_or(""));
                    Item {
                        kind: Kind::Script,
                        script_type: Some(entry.script_type.unwrap_or(ScriptType::Module)),
                        hash: Some(self.store.put(text.as_bytes())?),
                        meta,
                    }
                }
                Kind::Model => {
                    let hash = self
                        .live_mut(mapping)?
                        .uploaded_models
                        .get(&entry.key)
                        .cloned();
                    Item {
                        kind: Kind::Model,
                        script_type: None,
                        hash,
                        meta: None,
                    }
                }
            };
            tree.insert(entry.key, item);
        }
        Ok(tree)
    }

    /// Studio's tree for a mapping, with folders that hold nothing to sync left out.
    fn pruned(tree: &Tree) -> Tree {
        let mut tree = tree.clone();
        prune_folders(&mut tree);
        tree
    }

    /// Replaces what we know about Studio for this mapping.
    pub fn snapshot(&mut self, mapping: usize, entries: Vec<StudioEntry>) -> Result<()> {
        let tree = self.items_from_entries(mapping, entries)?;
        self.live_mut(mapping)?.studio = Some(tree);
        Ok(())
    }

    /// Applies a batch of Studio changes.
    pub fn changes(
        &mut self,
        mapping: usize,
        upserts: Vec<StudioEntry>,
        deletes: Vec<String>,
    ) -> Result<()> {
        let items = self.items_from_entries(mapping, upserts)?;
        let live = self.live_mut(mapping)?;
        let studio = live
            .studio
            .as_mut()
            .ok_or_else(|| anyhow!("no snapshot yet"))?;
        for key in deletes {
            studio.remove(&key);
        }
        studio.extend(items);
        Ok(())
    }

    pub fn take_ops(&mut self) -> Vec<Op> {
        self.session
            .as_mut()
            .map_or(Vec::new(), |s| s.ops.drain(..).collect())
    }

    /// The plugin finished an op. `ok` means Studio now matches. `kept` means a delete was refused
    /// because the instance holds things we don't sync. Anything else means it changed first and
    /// the new text is on its way as a change.
    pub fn ack(&mut self, op: u64, ok: bool, kept: bool) -> Result<()> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        let Some(flight) = session.in_flight.remove(&op) else {
            return Ok(());
        };
        let live = session
            .live
            .get_mut(flight.mapping)
            .ok_or_else(|| anyhow!("bad mapping"))?;
        let base = self.state.bases.entry(live.id.clone()).or_default();
        if ok {
            let studio = live.studio.get_or_insert_with(Tree::new);
            match &flight.target {
                Some(item) => {
                    studio.insert(flight.key.clone(), item.clone());
                    base.insert(flight.key, item.clone());
                }
                None => {
                    studio.remove(&flight.key);
                    base.remove(&flight.key);
                }
            }
        } else if kept {
            // Studio keeps the instance, so the disk side follows it again.
            base.remove(&flight.key);
        }
        self.store.save_state(&self.state)?;
        Ok(())
    }

    /// Compares every item and acts. Returns how many conflicts are new.
    pub fn reconcile(&mut self) -> Result<usize> {
        let count = self.session.as_ref().map_or(0, |s| s.live.len());
        let before = self.conflicts.len();
        let mut known: BTreeSet<(usize, String)> = self
            .conflicts
            .iter()
            .map(|c| (c.mapping, c.key.clone()))
            .collect();
        let mut new_conflicts = 0;
        for index in 0..count {
            new_conflicts += self.reconcile_mapping(index, &mut known)?;
        }
        self.store.save_state(&self.state)?;
        let _ = before;
        Ok(new_conflicts)
    }

    fn scan_live(&self, live: &Live) -> Result<DiskTree> {
        scan(
            &live.root,
            &ScanOptions {
                models: &live.models,
                ignore: &self.ignore,
            },
        )
    }

    fn reconcile_mapping(
        &mut self,
        index: usize,
        known: &mut BTreeSet<(usize, String)>,
    ) -> Result<usize> {
        let session = self.session.as_ref().ok_or_else(|| anyhow!("no session"))?;
        let live = &session.live[index];
        let Some(studio) = live.studio.as_ref().map(Self::pruned) else {
            return Ok(0);
        };
        // A model listed but not uploaded yet can't be compared.
        if studio
            .values()
            .any(|i| i.kind == Kind::Model && i.hash.is_none())
        {
            return Ok(0);
        }
        let mut busy: BTreeSet<String> = session
            .in_flight
            .values()
            .filter(|f| f.mapping == index)
            .map(|f| f.key.clone())
            .collect();
        let live_id = live.id.clone();
        let studio_root = live.mapping.studio.clone();
        let root = live.root.clone();
        let fresh = live.fresh;
        let format = live.models.format;

        let mut disk = self.scan_live(live)?;
        for warning in std::mem::take(&mut disk.warnings) {
            self.warn(warning);
        }
        // An unreadable model file waits until it's fixed, whatever Studio has.
        busy.extend(disk.broken.iter().cloned());
        let mut base = self.state.bases.remove(&live_id).unwrap_or_default();

        let keys: BTreeSet<String> = base
            .keys()
            .chain(studio.keys())
            .chain(disk.items.keys())
            .cloned()
            .collect();

        let mut to_disk: Vec<(String, Option<Item>)> = Vec::new();
        let mut to_studio: Vec<(String, Option<Item>)> = Vec::new();
        let mut reformat: Vec<String> = Vec::new();
        let mut new_conflicts = 0;

        for key in keys {
            if busy.contains(&key) {
                continue;
            }
            let b = base.get(&key).cloned();
            let s = studio.get(&key).cloned();
            let d = disk.items.get(&key).cloned();

            if s == d {
                match &s {
                    Some(item) => base.insert(key.clone(), item.clone()),
                    None => base.remove(&key),
                };
                // The same instances in a file of the other format: rewrite it in this one.
                let path = disk.paths.get(&key);
                if s.as_ref().is_some_and(|i| i.kind == Kind::Model)
                    && path.is_some_and(|p| {
                        p.extension().and_then(|e| e.to_str()) != Some(format.ext())
                    })
                {
                    reformat.push(key.clone());
                }
                self.conflicts
                    .retain(|c| !(c.mapping == index && c.key == key));
                known.remove(&(index, key));
                continue;
            }

            // Never synced, the same content, and the disk has no meta file: the folder was made by
            // something that can't store attributes (Script Sync can't), so Studio's are written.
            let b = match (&b, &s, &d) {
                (None, Some(si), Some(di))
                    if si.same_shape(di) && si.hash == di.hash && di.meta.is_none() =>
                {
                    Some(di.clone())
                }
                _ => b,
            };

            let conflict_reason = if b == s {
                (fresh && d.is_none()).then_some("Deleted on disk while Studio wasn't connected")
            } else if b == d {
                if fresh && s.is_none() {
                    Some("Deleted in Studio while the sync wasn't running")
                } else if fresh && self.state.is_past_version(&live_id, &key, &s) {
                    Some("Studio has an older version. Was the place closed without saving?")
                } else {
                    None
                }
            } else if b.is_none() {
                Some("Both sides have it, and they differ")
            } else if s.is_none() || d.is_none() {
                Some("Deleted on one side, changed on the other")
            } else {
                Some("Changed on both sides")
            };

            if let Some(reason) = conflict_reason {
                let conflict_key = (index, key.clone());
                if known.contains(&conflict_key) {
                    if let Some(c) = self
                        .conflicts
                        .iter_mut()
                        .find(|c| c.mapping == index && c.key == key)
                    {
                        c.studio = s;
                        c.disk = d;
                        c.disk_path = disk.paths.get(&key).cloned();
                    }
                } else {
                    let id = self.take_id();
                    self.conflicts.push(Conflict {
                        id,
                        mapping: index,
                        studio_root: studio_root.clone(),
                        key: key.clone(),
                        reason: reason.to_string(),
                        suggested: reason.contains("older version").then_some("disk"),
                        base: b,
                        studio: s,
                        disk: d,
                        disk_path: disk.paths.get(&key).cloned(),
                    });
                    known.insert(conflict_key);
                    new_conflicts += 1;
                }
                continue;
            }

            // An item with a conflict waits for the review, whatever happens to it meanwhile.
            if known.contains(&(index, key.clone())) {
                continue;
            }
            if b == s {
                to_studio.push((key, d));
            } else {
                to_disk.push((key, s));
            }
        }

        for key in reformat {
            if let Err(error) = Self::reformat_model(&root, &key, format, &mut disk) {
                self.warn(format!(
                    "could not convert {key} to .{}: {error:#}",
                    format.ext()
                ));
            }
        }
        let result = self.apply_to_disk(
            &root, &live_id, format, &studio, &mut disk, &mut base, to_disk,
        );
        self.state.bases.insert(live_id.clone(), base);
        self.state.remember_base(&live_id);
        for warning in result? {
            self.warn(warning);
        }
        self.queue_for_studio(index, &live_id, &disk, to_studio)?;

        if let Some(session) = self.session.as_mut() {
            session.live[index].fresh = false;
        }
        Ok(new_conflicts)
    }

    /// Rewrites a model file in the mapping's format, removing the old one.
    fn reformat_model(
        root: &Path,
        key: &str,
        format: ModelFormat,
        disk: &mut DiskTree,
    ) -> Result<()> {
        let old = disk
            .paths
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("no file"))?;
        let target = model_path(root, key, format);
        if target.exists() {
            bail!("{} is in the way", target.display());
        }
        fs::write(&target, modelfile::convert(&fs::read(&old)?, format)?)?;
        fs::remove_file(&old)?;
        disk.paths.insert(key.to_string(), target);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_to_disk(
        &self,
        root: &Path,
        live_id: &str,
        format: ModelFormat,
        studio: &Tree,
        disk: &mut DiskTree,
        base: &mut Tree,
        mut changes: Vec<(String, Option<Item>)>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        changes.sort_by_key(|(key, item)| {
            let d = depth(key) as i64;
            if item.is_some() {
                (0, d)
            } else {
                (1, -d)
            }
        });

        for (key, target) in changes {
            let previous = disk.items.get(&key).cloned();
            match &target {
                Some(item) => {
                    let has_children = studio.keys().any(|k| k.starts_with(&format!("{key}/")));
                    self.ensure_parent_layout(root, &key, disk, studio)?;
                    self.write_item(root, &key, item, has_children, format, disk)?;
                    let meta_file = disk.paths.get(&key).and_then(|p| meta_path(p, item.kind));
                    if let Some(meta_file) = meta_file {
                        if let Err(error) = write_meta(&meta_file, item.meta.as_deref()) {
                            warnings.push(format!("{error:#}"));
                        }
                    }
                    base.insert(key.clone(), item.clone());
                }
                None => {
                    if let Some(path) = disk.paths.remove(&key) {
                        let kind = previous.as_ref().map_or(Kind::Script, |i| i.kind);
                        if let Some(meta_file) = meta_path(&path, kind) {
                            let _ = fs::remove_file(meta_file);
                        }
                        match previous.as_ref().map(|i| i.kind) {
                            Some(Kind::Folder) => {
                                // Only empty directories go: stray files are never ours to delete.
                                let _ = fs::remove_dir(&path);
                            }
                            _ => {
                                fs::remove_file(&path)?;
                                // Directories that held only this go too, up to the mapping's own.
                                let mut dir = path.parent();
                                while let Some(current) =
                                    dir.filter(|d| d.starts_with(root) && *d != root)
                                {
                                    if fs::remove_dir(current).is_err() {
                                        break;
                                    }
                                    dir = current.parent();
                                }
                            }
                        }
                    }
                    disk.items.remove(&key);
                    base.remove(&key);
                }
            }
            self.store.log(&HistoryEntry {
                at: now_ms(),
                mapping: live_id.to_string(),
                key,
                direction: "toDisk".into(),
                change: if target.is_some() {
                    "update".into()
                } else {
                    "delete".into()
                },
                from: previous.and_then(|i| i.hash),
                to: target.and_then(|i| i.hash),
            })?;
        }
        Ok(warnings)
    }

    /// Moves a script's meta file along with the script.
    fn move_meta(from: &Path, to: &Path) -> Result<()> {
        let (Some(old), Some(new)) = (meta_path(from, Kind::Script), meta_path(to, Kind::Script))
        else {
            return Ok(());
        };
        if old != new && old.is_file() && !new.exists() {
            fs::rename(old, new)?;
        }
        Ok(())
    }

    /// A script that gains a child has to become `Name/init.luau`.
    fn ensure_parent_layout(
        &self,
        root: &Path,
        key: &str,
        disk: &mut DiskTree,
        studio: &Tree,
    ) -> Result<()> {
        let mut current = parent_key(key);
        while let Some(parent) = current {
            if let (Some(path), Some(item)) = (disk.paths.get(parent).cloned(), studio.get(parent))
            {
                if item.kind == Kind::Script && !is_init_file(&path) && path.is_file() {
                    let target = default_path(root, parent, item, true);
                    if let Some(dir) = target.parent() {
                        fs::create_dir_all(dir)?;
                    }
                    fs::rename(&path, &target)?;
                    Self::move_meta(&path, &target)?;
                    disk.paths.insert(parent.to_string(), target);
                }
            }
            current = parent_key(parent);
        }
        Ok(())
    }

    fn write_item(
        &self,
        root: &Path,
        key: &str,
        item: &Item,
        has_children: bool,
        format: ModelFormat,
        disk: &mut DiskTree,
    ) -> Result<()> {
        let existing = disk.paths.get(key).cloned();
        let same_kind = disk.items.get(key).is_some_and(|i| i.same_shape(item));
        let target = match (&existing, same_kind) {
            // A model's bytes are in the mapping's format, so its file is too.
            _ if item.kind == Kind::Model => model_path(root, key, format),
            (Some(path), true)
                if !has_children || is_init_file(path) || item.kind != Kind::Script =>
            {
                path.clone()
            }
            _ => default_path(root, key, item, has_children),
        };
        if let Some(old) = existing.filter(|old| *old != target) {
            if old.is_file() {
                fs::remove_file(&old)?;
                if item.kind == Kind::Script {
                    Self::move_meta(&old, &target)?;
                }
            }
        }
        match item.kind {
            Kind::Folder => fs::create_dir_all(&target)?,
            Kind::Script | Kind::Model => {
                if let Some(dir) = target.parent() {
                    fs::create_dir_all(dir)?;
                }
                let hash = item
                    .hash
                    .as_deref()
                    .ok_or_else(|| anyhow!("{key} has no content"))?;
                fs::write(&target, self.store.get(hash)?)?;
            }
        }
        disk.paths.insert(key.to_string(), target);
        disk.items.insert(key.to_string(), item.clone());
        Ok(())
    }

    fn queue_for_studio(
        &mut self,
        index: usize,
        live_id: &str,
        disk: &DiskTree,
        mut changes: Vec<(String, Option<Item>)>,
    ) -> Result<()> {
        changes.sort_by_key(|(key, item)| {
            let d = depth(key) as i64;
            if item.is_some() {
                (0, d)
            } else {
                (1, -d)
            }
        });

        let format = self
            .session
            .as_ref()
            .and_then(|s| s.live.get(index))
            .map_or(ModelFormat::Rbxm, |l| l.models.format);
        for (key, target) in changes {
            let id = self.take_id();
            let mut op = Op {
                id,
                mapping: index,
                key: key.clone(),
                action: if target.is_some() { "upsert" } else { "delete" },
                kind: target.as_ref().map(|i| i.kind),
                script_type: target.as_ref().and_then(|i| i.script_type),
                class_name: None,
                run_context: None,
                source: None,
                blob: None,
                attributes: None,
                tags: None,
            };
            if let Some(item) = &target {
                if item.kind != Kind::Model {
                    let (attributes, tags) = split_meta(item.meta.as_deref());
                    op.attributes = Some(attributes);
                    op.tags = Some(tags);
                }
                match item.kind {
                    Kind::Script => {
                        let script_type = item.script_type.unwrap_or(ScriptType::Module);
                        op.class_name = Some(script_type.class_name());
                        op.run_context = script_type.run_context();
                        let text = disk.sources.get(&key).cloned().unwrap_or_default();
                        self.store.put(text.as_bytes())?;
                        op.source = Some(text);
                    }
                    Kind::Model => {
                        // Studio reads only binary. The bytes are kept in the mapping's format too:
                        // that is the version Studio is about to have, by the disk's hash.
                        let path = disk
                            .paths
                            .get(&key)
                            .ok_or_else(|| anyhow!("{key} has no file"))?;
                        let bytes = fs::read(path)?;
                        self.store.put(&modelfile::convert(&bytes, format)?)?;
                        op.blob = Some(
                            self.store
                                .put(&modelfile::convert(&bytes, ModelFormat::Rbxm)?)?,
                        );
                    }
                    Kind::Folder => {}
                }
            }
            self.store.log(&HistoryEntry {
                at: now_ms(),
                mapping: live_id.to_string(),
                key: key.clone(),
                direction: "toStudio".into(),
                change: if target.is_some() {
                    "update".into()
                } else {
                    "delete".into()
                },
                from: None,
                to: target.as_ref().and_then(|i| i.hash.clone()),
            })?;
            if let Some(session) = self.session.as_mut() {
                session.in_flight.insert(
                    id,
                    InFlight {
                        mapping: index,
                        key,
                        target,
                    },
                );
                session.ops.push_back(op);
            }
        }
        Ok(())
    }

    pub fn conflicts(&self) -> &[Conflict] {
        &self.conflicts
    }

    fn conflict(&self, id: u64) -> Result<&Conflict> {
        self.conflicts
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| anyhow!("no such conflict"))
    }

    fn text_of(&self, item: &Option<Item>) -> Option<String> {
        let item = item.as_ref().filter(|i| i.kind == Kind::Script)?;
        let bytes = self.store.get(item.hash.as_deref()?).ok()?;
        String::from_utf8(bytes).ok()
    }

    fn disk_text(&self, conflict: &Conflict) -> Option<String> {
        conflict.disk.as_ref().filter(|i| i.kind == Kind::Script)?;
        let bytes = fs::read(conflict.disk_path.as_ref()?).ok()?;
        String::from_utf8(bytes).ok().map(|t| normalize_text(&t))
    }

    pub fn conflict_texts(&self, id: u64) -> Result<ConflictTexts> {
        let conflict = self.conflict(id)?;
        let base = self.text_of(&conflict.base);
        let studio = self.text_of(&conflict.studio);
        let disk = self.disk_text(conflict);
        let (mut merged, mut merged_with_markers) = (None, None);
        // A merge only means something when both sides moved away from the base. With one side
        // still on the base, it would just pick the other side, which may be the older one.
        if let (Some(studio), Some(disk), Some(original)) = (&studio, &disk, &base) {
            if original != studio && original != disk {
                match diffy::merge(original, disk, studio) {
                    Ok(text) => merged = Some(text),
                    Err(text) => merged_with_markers = Some(text),
                }
            }
        }
        let pretty = |item: &Option<Item>| {
            let meta = item.as_ref().and_then(|i| i.meta.as_deref())?;
            let value: serde_json::Value = serde_json::from_str(meta).ok()?;
            serde_json::to_string_pretty(&value).ok()
        };
        let (studio_meta, disk_meta) = (pretty(&conflict.studio), pretty(&conflict.disk));
        let (studio_meta, disk_meta) = if studio_meta == disk_meta {
            (None, None)
        } else {
            (studio_meta, disk_meta)
        };
        Ok(ConflictTexts {
            base,
            studio,
            disk,
            merged,
            merged_with_markers,
            studio_meta,
            disk_meta,
        })
    }

    /// Bytes of each side of a model conflict.
    pub fn conflict_models(&self, id: u64) -> Result<ModelSides> {
        let conflict = self.conflict(id)?;
        let studio = match conflict.studio.as_ref().and_then(|i| i.hash.as_deref()) {
            Some(hash) => Some(self.store.get(hash)?),
            None => None,
        };
        let disk = match conflict
            .disk_path
            .as_ref()
            .filter(|_| conflict.disk.is_some())
        {
            Some(path) => Some(fs::read(path)?),
            None => None,
        };
        Ok((studio, disk))
    }

    /// Settles a conflict. Each choice only moves the base, then `reconcile` does the work: taking
    /// a side means the other side now looks like the one that changed.
    ///
    /// `seen` is the (Studio hash, disk hash) the reviewer looked at. If either side has moved on,
    /// nothing happens and the review shows the new versions.
    pub fn resolve(
        &mut self,
        id: u64,
        choice: &str,
        text: Option<&str>,
        seen: (Option<&str>, Option<&str>),
    ) -> Result<()> {
        let conflict = self.conflict(id)?.clone();
        let hash = |item: &Option<Item>| item.as_ref().and_then(|i| i.hash.clone());
        if hash(&conflict.studio).as_deref() != seen.0 || hash(&conflict.disk).as_deref() != seen.1
        {
            bail!("this file changed while you were reviewing it");
        }

        let live_id = self.live_mut(conflict.mapping)?.id.clone();
        let root = self.live_mut(conflict.mapping)?.root.clone();
        let new_base = match choice {
            "studio" => conflict.disk.clone(),
            "disk" => conflict.studio.clone(),
            "merged" => {
                let text = normalize_text(text.ok_or_else(|| anyhow!("merged needs text"))?);
                let path = conflict.disk_path.clone().or_else(|| {
                    conflict
                        .studio
                        .as_ref()
                        .map(|item| default_path(&root, &conflict.key, item, false))
                });
                let path = path.ok_or_else(|| anyhow!("nowhere to write the merge"))?;
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(&path, text)?;
                // A merge covers the text. Attributes and tags are edited in Studio, so they come
                // from there.
                if let Some(meta_file) = meta_path(&path, Kind::Script) {
                    write_meta(
                        &meta_file,
                        conflict.studio.as_ref().and_then(|i| i.meta.as_deref()),
                    )?;
                }
                conflict.studio.clone()
            }
            other => bail!("unknown choice {other}"),
        };

        let base = self.state.bases.entry(live_id.clone()).or_default();
        match new_base {
            Some(item) => base.insert(conflict.key.clone(), item),
            None => base.remove(&conflict.key),
        };
        self.conflicts.retain(|c| c.id != id);
        self.settle_parent_folders(&conflict, choice, &live_id);
        self.reconcile()?;
        Ok(())
    }

    /// A folder in review goes the same way as the last thing inside it.
    fn settle_parent_folders(&mut self, conflict: &Conflict, choice: &str, live_id: &str) {
        let mut current = parent_key(&conflict.key).map(str::to_string);
        while let Some(key) = current {
            let prefix = if key.is_empty() {
                String::new()
            } else {
                format!("{key}/")
            };
            let inside = self.conflicts.iter().any(|c| {
                c.mapping == conflict.mapping && c.key != key && c.key.starts_with(&prefix)
            });
            if inside {
                return;
            }
            let found = self.conflicts.iter().position(|c| {
                let side = c.studio.as_ref().or(c.disk.as_ref());
                c.mapping == conflict.mapping
                    && c.key == key
                    && side.is_some_and(|i| i.kind == Kind::Folder)
            });
            if let Some(index) = found {
                let folder = self.conflicts.remove(index);
                let new_base = if choice == "studio" {
                    folder.disk
                } else {
                    folder.studio
                };
                let base = self.state.bases.entry(live_id.to_string()).or_default();
                match new_base {
                    Some(item) => base.insert(key.clone(), item),
                    None => base.remove(&key),
                };
            }
            current = parent_key(&key).map(str::to_string);
        }
    }

    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        self.config
            .mappings
            .iter()
            .map(|m| key_dir(&self.root, &m.path))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Kind;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        engine: Engine,
    }

    fn fixture() -> Fixture {
        fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "models": ["Assets"], "modelFormat": "rbxm", "instances": "listed" }"#,
        )
    }

    fn fixture_with(mapping: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("files")).unwrap();
        let config: Config =
            serde_json::from_str(&format!(r#"{{ "mappings": [ {mapping} ] }}"#)).unwrap();
        let mut engine = Engine::new(config, root.clone()).unwrap();
        engine.connect(0, "x").unwrap();
        Fixture {
            _dir: dir,
            root,
            engine,
        }
    }

    fn model(key: &str) -> StudioEntry {
        StudioEntry {
            key: key.into(),
            kind: Kind::Model,
            script_type: None,
            source: None,
            attributes: None,
            tags: None,
            group: false,
        }
    }

    fn script(key: &str, source: &str) -> StudioEntry {
        StudioEntry {
            key: key.into(),
            kind: Kind::Script,
            script_type: Some(ScriptType::Module),
            source: Some(source.into()),
            attributes: None,
            tags: None,
            group: false,
        }
    }

    fn with_meta(entry: StudioEntry, attributes: serde_json::Value, tags: &[&str]) -> StudioEntry {
        StudioEntry {
            attributes: Some(attributes),
            tags: Some(tags.iter().map(|t| t.to_string()).collect()),
            ..entry
        }
    }

    fn typed(key: &str, script_type: ScriptType, source: &str) -> StudioEntry {
        StudioEntry {
            script_type: Some(script_type),
            ..script(key, source)
        }
    }

    fn folder(key: &str) -> StudioEntry {
        StudioEntry {
            key: key.into(),
            kind: Kind::Folder,
            script_type: None,
            source: None,
            attributes: None,
            tags: None,
            group: false,
        }
    }

    impl Fixture {
        fn file(&self, relative: &str) -> PathBuf {
            self.root.join("files").join(relative)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.file(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.file(relative)).unwrap()
        }

        /// Plays the plugin: acknowledges every op as done.
        fn ack_all(&mut self) -> Vec<Op> {
            let ops = self.engine.take_ops();
            for op in &ops {
                self.engine.ack(op.id, true, false).unwrap();
            }
            ops
        }

        fn sync(&mut self, entries: Vec<StudioEntry>) -> usize {
            self.engine.snapshot(0, entries).unwrap();
            self.engine.reconcile().unwrap()
        }

        /// Both sides hold `text` at `A.luau`, with that as the base.
        fn agreed(&mut self, text: &str) {
            self.write("A.luau", text);
            assert_eq!(self.sync(vec![folder(""), script("A", text)]), 0);
            assert!(self.engine.take_ops().is_empty());
        }

        fn studio_edit(&mut self, text: &str) -> usize {
            self.engine
                .changes(0, vec![script("A", text)], vec![])
                .unwrap();
            self.engine.reconcile().unwrap()
        }
    }

    #[test]
    fn first_sync_copies_each_side_to_the_other() {
        let mut f = fixture();
        f.write("Disk.luau", "disk");
        assert_eq!(f.sync(vec![folder(""), script("Studio", "studio")]), 0);

        assert_eq!(f.read("Studio.luau"), "studio");
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].key, "Disk");
        assert_eq!(ops[0].source.as_deref(), Some("disk"));
    }

    #[test]
    fn a_new_mapping_with_files_only_in_studio_fills_the_folder() {
        let mut f = fixture();
        fs::remove_dir(f.root.join("files")).unwrap();
        let conflicts = f.sync(vec![
            folder(""),
            folder("Sub"),
            script("Sub/Mod", "return 1"),
            script("Parent", "parent"),
            script("Parent/Child", "child"),
            typed("Run", ScriptType::Client, "print(1)"),
            typed("Serve", ScriptType::Server, "print(2)"),
        ]);
        assert_eq!(conflicts, 0);
        assert_eq!(f.read("Sub/Mod.luau"), "return 1");
        assert_eq!(f.read("Parent/init.luau"), "parent");
        assert_eq!(f.read("Parent/Child.luau"), "child");
        assert_eq!(f.read("Run.client.luau"), "print(1)");
        assert_eq!(f.read("Serve.server.luau"), "print(2)");
        assert!(f.engine.take_ops().is_empty(), "Studio is left alone");
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn a_new_mapping_with_files_only_on_disk_fills_studio() {
        let mut f = fixture();
        f.write("Sub/Mod.luau", "return 1");
        f.write("Parent/init.luau", "parent");
        f.write("Parent/Child.luau", "child");
        f.write("Run.client.luau", "print(1)");
        // The root instance doesn't exist in Studio either.
        assert_eq!(f.sync(vec![]), 0);

        let ops = f.engine.take_ops();
        let keys: Vec<_> = ops.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            ["", "Parent", "Run", "Sub", "Parent/Child", "Sub/Mod"],
            "parents first"
        );
        assert!(
            ops.iter().all(|o| o.action == "upsert"),
            "nothing is deleted on either side"
        );
        let run = ops.iter().find(|o| o.key == "Run").unwrap();
        assert_eq!(run.class_name, Some("LocalScript"));
        assert_eq!(f.read("Parent/init.luau"), "parent");

        for op in &ops {
            f.engine.ack(op.id, true, false).unwrap();
        }
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn a_new_mapping_with_different_content_on_both_sides_asks() {
        let mut f = fixture();
        f.write("A.luau", "disk");
        assert_eq!(f.sync(vec![folder(""), script("A", "studio")]), 1);
        assert_eq!(f.read("A.luau"), "disk");
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn script_sync_file_names_stay_as_they_are() {
        let mut f = fixture();
        f.write("Old.local.luau", "one");
        f.write("Svc.legacy.luau", "two");
        f.sync(vec![
            folder(""),
            typed("Old", ScriptType::Client, "one"),
            typed("Svc", ScriptType::Server, "two"),
        ]);
        assert!(
            f.engine.take_ops().is_empty(),
            "the types match, so nothing to do"
        );

        f.engine
            .changes(0, vec![typed("Old", ScriptType::Client, "edited")], vec![])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("Old.local.luau"), "edited");
        assert!(!f.file("Old.client.luau").exists());
    }

    #[test]
    fn matching_sides_need_nothing() {
        let mut f = fixture();
        f.agreed("same");
        assert_eq!(f.engine.reconcile().unwrap(), 0);
        assert!(f.engine.take_ops().is_empty());
        assert!(f.engine.conflicts().is_empty());
    }

    #[test]
    fn a_studio_edit_reaches_the_file_and_does_not_echo_back() {
        let mut f = fixture();
        f.agreed("one");
        assert_eq!(f.studio_edit("two"), 0);
        assert_eq!(f.read("A.luau"), "two");
        // The file watcher fires for our own write: nothing goes back to Studio.
        f.engine.reconcile().unwrap();
        assert!(
            f.engine.take_ops().is_empty(),
            "the edit must not be echoed to Studio"
        );
    }

    #[test]
    fn a_disk_edit_reaches_studio_once() {
        let mut f = fixture();
        f.agreed("one");
        f.write("A.luau", "two");
        f.engine.reconcile().unwrap();
        // Still waiting for the plugin: a second event must not send it again.
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].source.as_deref(), Some("two"));
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn line_endings_alone_are_not_a_change() {
        let mut f = fixture();
        f.agreed("a\nb\n");
        f.write("A.luau", "a\r\nb\r\n");
        assert_eq!(f.engine.reconcile().unwrap(), 0);
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn edits_on_both_sides_become_a_conflict_and_leave_both_alone() {
        let mut f = fixture();
        f.agreed("base");
        f.write("A.luau", "disk");
        assert_eq!(f.studio_edit("studio"), 1);
        assert_eq!(f.read("A.luau"), "disk");
        assert!(f.engine.take_ops().is_empty());
        // Reconciling again doesn't report it twice.
        assert_eq!(f.engine.reconcile().unwrap(), 0);
        assert_eq!(f.engine.conflicts().len(), 1);
    }

    #[test]
    fn a_conflict_blocks_only_its_own_file() {
        let mut f = fixture();
        f.write("A.luau", "base");
        f.write("B.luau", "b");
        f.sync(vec![folder(""), script("A", "base"), script("B", "b")]);
        f.write("A.luau", "disk");
        assert_eq!(f.studio_edit("studio"), 1);

        f.engine
            .changes(0, vec![script("B", "b2")], vec![])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("B.luau"), "b2");
    }

    fn conflicted() -> (Fixture, u64, String, String) {
        let mut f = fixture();
        f.agreed("l1\nl2\nl3\nl4\nl5\n");
        f.write("A.luau", "l1\nl2\nl3\nl4\ndisk\n");
        f.studio_edit("studio\nl2\nl3\nl4\nl5\n");
        let c = f.engine.conflicts()[0].clone();
        let (s, d) = (
            c.studio.unwrap().hash.unwrap(),
            c.disk.unwrap().hash.unwrap(),
        );
        (f, c.id, s, d)
    }

    #[test]
    fn keep_studio_overwrites_the_file() {
        let (mut f, id, s, d) = conflicted();
        f.engine
            .resolve(id, "studio", None, (Some(&s), Some(&d)))
            .unwrap();
        assert_eq!(f.read("A.luau"), "studio\nl2\nl3\nl4\nl5\n");
        assert!(f.engine.take_ops().is_empty());
        assert!(f.engine.conflicts().is_empty());
    }

    #[test]
    fn keep_disk_goes_to_studio() {
        let (mut f, id, s, d) = conflicted();
        f.engine
            .resolve(id, "disk", None, (Some(&s), Some(&d)))
            .unwrap();
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].source.as_deref(), Some("l1\nl2\nl3\nl4\ndisk\n"));
    }

    #[test]
    fn separate_edits_merge_cleanly_and_reach_both_sides() {
        let (mut f, id, s, d) = conflicted();
        let texts = f.engine.conflict_texts(id).unwrap();
        let merged = texts.merged.expect("edits on different lines merge");
        assert_eq!(merged, "studio\nl2\nl3\nl4\ndisk\n");
        f.engine
            .resolve(id, "merged", Some(&merged), (Some(&s), Some(&d)))
            .unwrap();
        assert_eq!(f.read("A.luau"), merged);
        let ops = f.ack_all();
        assert_eq!(ops[0].source.as_deref(), Some(merged.as_str()));
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn overlapping_edits_give_markers_not_a_merge() {
        let mut f = fixture();
        f.agreed("line\n");
        f.write("A.luau", "disk\n");
        f.studio_edit("studio\n");
        let id = f.engine.conflicts()[0].id;
        let texts = f.engine.conflict_texts(id).unwrap();
        assert!(texts.merged.is_none());
        assert!(texts.merged_with_markers.unwrap().contains("<<<<<<<"));
    }

    #[test]
    fn a_resolve_for_stale_versions_is_refused() {
        let (mut f, id, s, d) = conflicted();
        f.engine
            .changes(0, vec![script("A", "newer")], vec![])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert!(f
            .engine
            .resolve(id, "studio", None, (Some(&s), Some(&d)))
            .is_err());
        assert_eq!(f.engine.conflicts().len(), 1);
        assert_eq!(f.read("A.luau"), "l1\nl2\nl3\nl4\ndisk\n");
    }

    #[test]
    fn a_conflict_clears_itself_when_both_sides_end_up_equal() {
        let mut f = fixture();
        f.agreed("base");
        f.write("A.luau", "same");
        f.studio_edit("other");
        assert_eq!(f.engine.conflicts().len(), 1);
        f.studio_edit("same");
        assert!(f.engine.conflicts().is_empty());
    }

    #[test]
    fn deletions_found_on_connect_go_to_review() {
        let mut f = fixture();
        f.agreed("text");
        fs::remove_file(f.file("A.luau")).unwrap();
        f.engine.connect(0, "x").unwrap();
        assert_eq!(f.sync(vec![folder(""), script("A", "text")]), 1);
        assert!(
            f.engine.take_ops().is_empty(),
            "nothing is deleted in Studio without review"
        );
    }

    #[test]
    fn reopening_an_unsaved_place_does_not_overwrite_newer_files() {
        let mut f = fixture();
        f.agreed("old");
        f.studio_edit("new");
        assert_eq!(f.read("A.luau"), "new");
        f.engine.reconcile().unwrap();

        // Studio closed without saving: the place comes back with the old text.
        f.engine.connect(0, "x").unwrap();
        assert_eq!(f.sync(vec![folder(""), script("A", "old")]), 1);
        assert_eq!(f.read("A.luau"), "new", "the newer file is kept");
        let conflict = f.engine.conflicts()[0].clone();
        assert!(conflict.reason.contains("older version"));
        assert_eq!(conflict.suggested, Some("disk"));
        let texts = f.engine.conflict_texts(conflict.id).unwrap();
        assert!(
            texts.merged.is_none(),
            "no merge may offer to bring the old text back"
        );
        assert!(texts.merged_with_markers.is_none());
    }

    #[test]
    fn a_real_studio_edit_made_offline_still_syncs_on_connect() {
        let mut f = fixture();
        f.agreed("old");
        f.engine.connect(0, "x").unwrap();
        assert_eq!(
            f.sync(vec![folder(""), script("A", "edited while offline")]),
            0
        );
        assert_eq!(f.read("A.luau"), "edited while offline");
    }

    #[test]
    fn going_back_to_an_old_version_while_connected_is_just_an_edit() {
        let mut f = fixture();
        f.agreed("old");
        f.studio_edit("new");
        f.engine.reconcile().unwrap();
        assert_eq!(f.studio_edit("old"), 0);
        assert_eq!(
            f.read("A.luau"),
            "old",
            "an undo syncs like any other change"
        );
    }

    #[test]
    fn a_folder_in_review_settles_with_the_last_thing_inside_it() {
        let mut f = fixture();
        f.write("Dir/A.luau", "a");
        f.write("Dir/B.luau", "b");
        f.sync(vec![
            folder(""),
            folder("Dir"),
            script("Dir/A", "a"),
            script("Dir/B", "b"),
        ]);

        // The place comes back without the folder at all.
        f.engine.connect(0, "x").unwrap();
        assert_eq!(f.sync(vec![folder("")]), 3);

        let hashes = |c: &Conflict| {
            (
                c.studio.as_ref().and_then(|i| i.hash.clone()),
                c.disk.as_ref().and_then(|i| i.hash.clone()),
            )
        };
        for name in ["Dir/A", "Dir/B"] {
            let c = f
                .engine
                .conflicts()
                .iter()
                .find(|c| c.key == name)
                .unwrap()
                .clone();
            let (s, d) = hashes(&c);
            f.engine
                .resolve(c.id, "disk", None, (s.as_deref(), d.as_deref()))
                .unwrap();
        }
        assert!(
            f.engine.conflicts().is_empty(),
            "the folder went with its contents"
        );
        let ops = f.engine.take_ops();
        let keys: Vec<_> = ops.iter().map(|o| o.key.as_str()).collect();
        assert!(
            keys.contains(&"Dir"),
            "and it's recreated in Studio: {keys:?}"
        );
    }

    #[test]
    fn a_first_sync_adds_studio_attributes_to_files_that_never_had_them() {
        let mut f = fixture();
        f.write("A.luau", "same");
        let entry = with_meta(script("A", "same"), serde_json::json!({ "Weight": 1 }), &[]);
        assert_eq!(f.sync(vec![folder(""), entry]), 0, "not a conflict");
        assert!(f.read("A.meta.json").contains("Weight"));
    }

    #[test]
    fn a_first_sync_with_different_attributes_on_both_sides_asks() {
        let mut f = fixture();
        f.write("A.luau", "same");
        f.write("A.meta.json", r#"{ "attributes": { "Weight": 2 } }"#);
        let entry = with_meta(script("A", "same"), serde_json::json!({ "Weight": 1 }), &[]);
        assert_eq!(f.sync(vec![folder(""), entry]), 1);
        let id = f.engine.conflicts()[0].id;
        let texts = f.engine.conflict_texts(id).unwrap();
        assert!(texts.studio_meta.unwrap().contains("1"));
        assert!(texts.disk_meta.unwrap().contains("2"));
    }

    #[test]
    fn studio_attributes_and_tags_land_in_rojo_meta_files() {
        let mut f = fixture();
        f.sync(vec![
            folder(""),
            with_meta(
                folder("Dir"),
                serde_json::json!({ "Speed": 2.5 }),
                &["Zone"],
            ),
            with_meta(
                script("Dir/A", "a"),
                serde_json::json!({ "Tint": { "Color3": [1, 0, 0] } }),
                &[],
            ),
        ]);
        let dir_meta: serde_json::Value =
            serde_json::from_str(&f.read("Dir/init.meta.json")).unwrap();
        assert_eq!(
            dir_meta,
            serde_json::json!({ "attributes": { "Speed": 2.5 }, "properties": { "Tags": ["Zone"] } })
        );
        let script_meta: serde_json::Value =
            serde_json::from_str(&f.read("Dir/A.meta.json")).unwrap();
        assert_eq!(
            script_meta,
            serde_json::json!({ "attributes": { "Tint": { "Color3": [1, 0, 0] } } })
        );
        assert!(f.engine.take_ops().is_empty());
        f.engine.reconcile().unwrap();
        assert!(
            f.engine.take_ops().is_empty(),
            "reading the files back changes nothing"
        );
    }

    #[test]
    fn an_attribute_edited_on_disk_goes_to_studio() {
        let mut f = fixture();
        f.write("A.luau", "a");
        f.sync(vec![
            folder(""),
            with_meta(script("A", "a"), serde_json::json!({ "Weight": 1 }), &[]),
        ]);
        f.write(
            "A.meta.json",
            r#"{ "attributes": { "Weight": 5, "New": "yes" }, "properties": { "Tags": ["T"] } }"#,
        );
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        let attributes = ops[0].attributes.as_ref().unwrap();
        assert_eq!(attributes["Weight"], 5);
        assert_eq!(attributes["New"], "yes");
        assert_eq!(ops[0].tags.as_deref(), Some(&["T".to_string()][..]));
    }

    #[test]
    fn removing_the_last_attribute_in_studio_removes_the_meta_file() {
        let mut f = fixture();
        f.write("A.luau", "a");
        f.sync(vec![
            folder(""),
            with_meta(script("A", "a"), serde_json::json!({ "Weight": 1 }), &[]),
        ]);
        assert!(f.file("A.meta.json").exists());
        f.engine.changes(0, vec![script("A", "a")], vec![]).unwrap();
        f.engine.reconcile().unwrap();
        assert!(!f.file("A.meta.json").exists());
    }

    #[test]
    fn studio_float_noise_does_not_rewrite_the_file() {
        let mut f = fixture();
        f.write("A.luau", "a");
        f.write(
            "A.meta.json",
            r#"{ "attributes": { "Tint": { "Color3": [0.1, 0.2, 0.3] } } }"#,
        );
        let studio = serde_json::json!({ "Tint": { "Color3": [0.10000000149011612, 0.20000000298023224, 0.30000001192092896] } });
        assert_eq!(
            f.sync(vec![folder(""), with_meta(script("A", "a"), studio, &[])]),
            0
        );
        assert!(f.engine.take_ops().is_empty());
        assert!(
            f.read("A.meta.json").contains("[0.1, 0.2, 0.3]"),
            "the file is untouched"
        );
    }

    #[test]
    fn the_meta_file_moves_when_a_script_gains_a_child() {
        let mut f = fixture();
        f.write("A.luau", "a");
        f.sync(vec![
            folder(""),
            with_meta(script("A", "a"), serde_json::json!({ "X": 1 }), &[]),
        ]);
        f.engine
            .changes(0, vec![script("A/Child", "c")], vec![])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert!(f.file("A/init.meta.json").exists());
        assert!(!f.file("A.meta.json").exists());
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn deletions_while_connected_sync() {
        let mut f = fixture();
        f.agreed("text");
        fs::remove_file(f.file("A.luau")).unwrap();
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].action, "delete");

        f.write("B.luau", "b");
        f.engine.reconcile().unwrap();
        f.ack_all();
        f.engine.changes(0, vec![], vec!["B".into()]).unwrap();
        f.engine.reconcile().unwrap();
        assert!(!f.file("B.luau").exists());
    }

    #[test]
    fn a_stale_op_turns_into_a_conflict() {
        let mut f = fixture();
        f.agreed("base");
        f.write("A.luau", "disk");
        f.engine.reconcile().unwrap();
        let op = f.engine.take_ops().remove(0);
        // Studio had unsent edits: the plugin refuses, reports the text, then acks.
        f.engine
            .changes(0, vec![script("A", "typed")], vec![])
            .unwrap();
        f.engine.ack(op.id, false, false).unwrap();
        assert_eq!(f.engine.reconcile().unwrap(), 1);
    }

    #[test]
    fn a_refused_folder_delete_settles_instead_of_looping() {
        let mut f = fixture();
        f.write("Dir/A.luau", "a");
        let dir = with_meta(folder("Dir"), serde_json::json!({ "On": true }), &[]);
        f.sync(vec![folder(""), dir.clone(), script("Dir/A", "a")]);
        fs::remove_dir_all(f.file("Dir")).unwrap();
        f.engine.reconcile().unwrap();

        let ops = f.engine.take_ops();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].key, "Dir/A", "children go first");
        f.engine.ack(ops[0].id, true, false).unwrap();
        // Studio's folder still holds something that isn't synced, so the plugin keeps it.
        f.engine.ack(ops[1].id, false, true).unwrap();
        f.engine.reconcile().unwrap();
        assert!(
            f.file("Dir/init.meta.json").is_file(),
            "the folder Studio kept comes back on disk"
        );
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn deleting_the_last_script_removes_the_emptied_directories() {
        let mut f = fixture();
        f.write("A/B/C.luau", "c");
        f.write("Keep.luau", "k");
        f.sync(vec![
            folder(""),
            folder("A"),
            folder("A/B"),
            script("A/B/C", "c"),
            script("Keep", "k"),
        ]);
        f.engine.changes(0, vec![], vec!["A/B/C".into()]).unwrap();
        f.engine.reconcile().unwrap();
        assert!(!f.file("A").exists(), "A and A/B held only C");
        assert!(f.file("Keep.luau").exists());
        assert!(
            f.root.join("files").is_dir(),
            "the mapping's own folder stays"
        );
    }

    #[test]
    fn a_directory_with_notes_left_in_it_stays() {
        let mut f = fixture();
        f.write("A/C.luau", "c");
        f.write("A/notes.md", "mine");
        f.sync(vec![folder(""), folder("A"), script("A/C", "c")]);
        f.engine.changes(0, vec![], vec!["A/C".into()]).unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("A/notes.md"), "mine");
    }

    #[test]
    fn a_script_that_gains_a_child_moves_to_init() {
        let mut f = fixture();
        f.agreed("parent");
        f.engine
            .changes(0, vec![script("A/Child", "child")], vec![])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("A/init.luau"), "parent");
        assert_eq!(f.read("A/Child.luau"), "child");
        assert!(!f.file("A.luau").exists());
        f.engine.reconcile().unwrap();
        assert!(f.engine.take_ops().is_empty());
    }

    #[test]
    fn a_rename_moves_the_file() {
        let mut f = fixture();
        f.agreed("text");
        f.engine
            .changes(0, vec![script("B", "text")], vec!["A".into()])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert!(!f.file("A.luau").exists());
        assert_eq!(f.read("B.luau"), "text");
    }

    #[test]
    fn stray_files_survive_a_folder_delete() {
        let mut f = fixture();
        f.write("Dir/notes.md", "keep me");
        f.write("Dir/A.luau", "a");
        f.sync(vec![folder(""), folder("Dir"), script("Dir/A", "a")]);
        f.engine
            .changes(0, vec![], vec!["Dir".into(), "Dir/A".into()])
            .unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("Dir/notes.md"), "keep me");
    }

    #[test]
    fn unsafe_names_are_skipped_with_a_warning() {
        let mut f = fixture();
        f.sync(vec![
            folder(""),
            script("Bad:Name", "x"),
            script("init", "y"),
            script("Fine", "z"),
        ]);
        assert_eq!(f.read("Fine.luau"), "z");
        assert_eq!(f.engine.warnings.len(), 2);
        assert_eq!(fs::read_dir(f.root.join("files")).unwrap().count(), 1);
    }

    #[test]
    fn names_that_differ_only_by_case_are_not_both_written() {
        let mut f = fixture();
        f.sync(vec![folder(""), script("Name", "a"), script("NAME", "b")]);
        assert_eq!(f.engine.warnings.len(), 1);
        assert_eq!(fs::read_dir(f.root.join("files")).unwrap().count(), 1);
    }

    #[test]
    fn models_sync_as_bytes() {
        use crate::modelfile::tests::sample_with;
        let mut f = fixture();
        f.engine
            .upload_model(0, "Assets", &sample_with(0.1))
            .unwrap();
        f.sync(vec![folder(""), model("Assets")]);
        assert_eq!(
            fs::read(f.file("Assets.rbxm")).unwrap(),
            sample_with(0.1),
            "Studio's bytes, unchanged"
        );

        fs::write(f.file("Assets.rbxm"), sample_with(0.2)).unwrap();
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(ops[0].kind, Some(Kind::Model));
        assert_eq!(
            f.engine.store.get(ops[0].blob.as_ref().unwrap()).unwrap(),
            sample_with(0.2)
        );
    }

    #[test]
    fn auto_mode_saves_unlisted_instances_as_xml_and_sends_studio_binary() {
        use crate::modelfile::{convert, tests::sample_with};
        let mut f = fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto", "modelFormat": "rbxmx", "groupInstances": false }"#,
        );
        f.engine
            .upload_model(0, "Preset/Sky", &sample_with(0.1))
            .unwrap();
        f.sync(vec![
            folder(""),
            folder("Preset"),
            script("Preset/Lighting", "return 1"),
            model("Preset/Sky"),
        ]);
        let xml = f.read("Preset/Sky.rbxmx");
        assert!(xml.starts_with("<roblox"), "{xml}");
        assert_eq!(
            xml.as_bytes(),
            convert(&sample_with(0.1), ModelFormat::Rbxmx).unwrap()
        );
        assert!(!f.file("Preset/Sky.rbxm").exists());
        assert_eq!(
            f.read("Preset/Lighting.luau"),
            "return 1",
            "scripts stay text"
        );

        // Studio sends the same bytes again: nothing to do.
        f.engine
            .upload_model(0, "Preset/Sky", &sample_with(0.1))
            .unwrap();
        assert_eq!(f.engine.reconcile().unwrap(), 0);
        assert!(f.engine.take_ops().is_empty());

        // An edit in the XML reaches Studio as binary.
        f.write(
            "Preset/Sky.rbxmx",
            std::str::from_utf8(&convert(&sample_with(0.9), ModelFormat::Rbxmx).unwrap()).unwrap(),
        );
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(ops.len(), 1);
        let blob = f.engine.store.get(ops[0].blob.as_ref().unwrap()).unwrap();
        assert!(blob.starts_with(b"<roblox!"), "Studio reads only binary");
        assert_eq!(
            convert(&blob, ModelFormat::Rbxmx).unwrap(),
            convert(&sample_with(0.9), ModelFormat::Rbxmx).unwrap()
        );
        // Studio now matches, so a fresh upload of the same instances changes nothing on disk.
        let before = f.read("Preset/Sky.rbxmx");
        f.engine
            .upload_model(0, "Preset/Sky", &sample_with(0.9))
            .unwrap();
        f.engine.reconcile().unwrap();
        assert_eq!(f.read("Preset/Sky.rbxmx"), before);
    }

    fn group(key: &str) -> StudioEntry {
        StudioEntry {
            group: true,
            ..model(key)
        }
    }

    #[test]
    fn a_folders_loose_instances_share_one_file() {
        use crate::modelfile::{convert, decode, tests::sample_group};
        let mut f = fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto" }"#,
        );
        f.engine
            .upload_model(0, "Preset/init", &sample_group(0.1))
            .unwrap();
        f.sync(vec![
            folder(""),
            folder("Preset"),
            script("Preset/Lighting", "return 1"),
            group("Preset/init"),
        ]);
        assert_eq!(
            fs::read(f.file("Preset/init.rbxmx")).unwrap(),
            convert(&sample_group(0.1), ModelFormat::Rbxmx).unwrap()
        );
        assert_eq!(f.read("Preset/Lighting.luau"), "return 1");
        assert!(f.engine.warnings.is_empty(), "{:?}", f.engine.warnings);

        // An edit in the file reaches Studio as one binary holding both instances.
        f.write(
            "Preset/init.rbxmx",
            std::str::from_utf8(&convert(&sample_group(0.7), ModelFormat::Rbxmx).unwrap()).unwrap(),
        );
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!((ops.len(), ops[0].key.as_str()), (1, "Preset/init"));
        let blob = f.engine.store.get(ops[0].blob.as_ref().unwrap()).unwrap();
        assert!(blob.starts_with(b"<roblox!"));
        assert_eq!(decode(&blob).unwrap().root().children().len(), 2);

        // Deleting the file removes them from Studio.
        fs::remove_file(f.file("Preset/init.rbxmx")).unwrap();
        f.engine.reconcile().unwrap();
        let ops = f.ack_all();
        assert_eq!(
            (ops[0].action, ops[0].key.as_str()),
            ("delete", "Preset/init")
        );
    }

    #[test]
    fn an_instance_named_init_is_not_a_group() {
        let mut f = fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto" }"#,
        );
        f.engine
            .upload_model(0, "init", &crate::modelfile::tests::sample())
            .unwrap();
        f.sync(vec![folder(""), model("init")]);
        assert!(!f.file("init.rbxmx").exists());
        assert!(f.engine.warnings.iter().any(|w| w.contains("init")));
    }

    #[test]
    fn listed_mode_refuses_unlisted_models_and_exclude_wins() {
        let mut f = fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto", "exclude": ["Map"] }"#,
        );
        assert!(f
            .engine
            .upload_model(0, "Map", &crate::modelfile::tests::sample())
            .is_err());
        let mut g = fixture();
        assert!(g
            .engine
            .upload_model(0, "Other", &crate::modelfile::tests::sample())
            .is_err());
        assert!(g
            .engine
            .upload_model(0, "Assets", &crate::modelfile::tests::sample())
            .is_ok());
    }

    #[test]
    fn a_new_model_format_converts_existing_files() {
        use crate::modelfile::{convert, tests::sample_with};
        let mut f = fixture();
        f.engine
            .upload_model(0, "Assets", &sample_with(0.3))
            .unwrap();
        f.sync(vec![folder(""), model("Assets")]);
        assert!(f.file("Assets.rbxm").exists());

        // The same project, now asking for XML.
        let config: Config = serde_json::from_str(
            r#"{ "mappings": [ { "studio": "ReplicatedStorage/Kit", "path": "files", "models": ["Assets"], "modelFormat": "rbxmx" } ] }"#,
        )
        .unwrap();
        let mut engine = Engine::new(config, f.root.clone()).unwrap();
        engine.connect(0, "x").unwrap();
        engine.upload_model(0, "Assets", &sample_with(0.3)).unwrap();
        engine
            .snapshot(0, vec![folder(""), model("Assets")])
            .unwrap();
        assert_eq!(
            engine.reconcile().unwrap(),
            0,
            "the same instances: not a conflict"
        );
        assert!(engine.take_ops().is_empty());
        assert!(!f.file("Assets.rbxm").exists());
        assert_eq!(
            fs::read(f.file("Assets.rbxmx")).unwrap(),
            convert(&sample_with(0.3), ModelFormat::Rbxmx).unwrap()
        );
    }

    #[test]
    fn an_unreadable_model_file_is_left_alone_on_both_sides() {
        let mut f = fixture_with(
            r#"{ "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto", "modelFormat": "rbxmx", "groupInstances": false }"#,
        );
        f.engine
            .upload_model(0, "Sky", &crate::modelfile::tests::sample())
            .unwrap();
        f.sync(vec![folder(""), model("Sky")]);
        f.write("Sky.rbxmx", "<roblox><Item class=");
        f.engine.reconcile().unwrap();
        assert!(
            f.engine.take_ops().is_empty(),
            "no delete or upsert goes to Studio"
        );
        assert_eq!(
            f.read("Sky.rbxmx"),
            "<roblox><Item class=",
            "and the file isn't overwritten"
        );
        assert!(f
            .engine
            .warnings
            .iter()
            .any(|w| w.contains("can't be read")));
    }

    #[test]
    fn the_base_survives_a_restart() {
        let mut f = fixture();
        f.agreed("text");
        let config = f.engine.config.clone();
        let mut engine = Engine::new(config, f.root.clone()).unwrap();
        engine.connect(0, "x").unwrap();
        // Studio changed while the sync was off, the file didn't: not a conflict.
        engine
            .snapshot(0, vec![folder(""), script("A", "changed")])
            .unwrap();
        assert_eq!(engine.reconcile().unwrap(), 0);
        assert_eq!(fs::read_to_string(f.file("A.luau")).unwrap(), "changed");
    }

    #[test]
    fn history_records_what_synced() {
        let mut f = fixture();
        f.agreed("one");
        f.studio_edit("two");
        let history = f.engine.store.history(10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].direction, "toDisk");
        assert_eq!(history[0].key, "A");
    }
}

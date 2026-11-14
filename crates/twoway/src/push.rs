//! A one-time push of the mapped folders into a Studio, for `rojo studio push`.
//!
//! The live sync decides with three versions (Studio, disk, and what both last agreed on) and
//! writes either way. A push only writes Studio, and it may target any Studio, such as a test
//! window opened from an old place file. So the files win wherever Studio holds a version the
//! sync knows (the agreed one, or one agreed on before): that Studio is simply behind. Where Studio
//! holds something the sync has never seen, it may be someone's work, so that item is a conflict:
//! reported, and left alone unless the caller says which side wins.
//!
//! Nothing here writes the files or `.twoway/`: the agreed state belongs to the live sync.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::config::{Config, Mapping};
use crate::engine::{read_agreed, studio_items, Op, StudioEntry};
use crate::modeldiff;
use crate::modelfile::{self, ModelFormat};
use crate::tree::{
    depth, hash_bytes, normalize_text, prune_folders, scan, split_meta, Item, Kind, ModelRule,
    ScanOptions, ScriptType,
};

/// Which side wins a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Studio keeps what it has.
    Studio,
    /// The file goes to Studio (or, for a file that's gone, the instance is removed).
    Disk,
}

impl std::str::FromStr for Side {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "studio" => Ok(Side::Studio),
            "disk" => Ok(Side::Disk),
            other => anyhow::bail!("a side is studio or disk, not {other}"),
        }
    }
}

/// What Studio has under one mapping, as the Studio module reports it.
#[derive(Debug, Default)]
pub struct StudioSide {
    pub entries: Vec<StudioEntry>,
    /// Each model's bytes (binary, as Studio saves them), by key.
    pub models: BTreeMap<String, Vec<u8>>,
}

/// One change to make in Studio: the op the Studio module carries out, with a model's bytes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedOp {
    #[serde(flatten)]
    pub op: Op,
    /// `create`, `update` or `delete`.
    pub change: &'static str,
    /// The instance's full path in Studio.
    pub path: String,
    /// Binary model bytes, for a model.
    #[serde(skip)]
    pub blob: Option<Vec<u8>>,
    /// Studio's text when it was read, so text typed since isn't overwritten.
    #[serde(skip)]
    pub studio_source: Option<String>,
}

/// An item left alone because Studio holds something the sync doesn't know.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushConflict {
    pub path: String,
    pub reason: &'static str,
    /// The side to pick when one is clearly right.
    pub suggested: Option<Side>,
    pub studio: Option<Item>,
    pub disk: Option<Item>,
    pub file: Option<PathBuf>,
    /// For a script: the file's text changed into Studio's, as a unified diff.
    pub diff: Option<String>,
}

/// What a push does to one mapping.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingPlan {
    pub studio: String,
    pub ops: Vec<PlannedOp>,
    pub conflicts: Vec<PushConflict>,
    /// Conflicts settled by the caller's choice of Studio: left as they are.
    pub kept: Vec<String>,
    /// Instances only Studio has that the sync never saw: not the files', so left alone.
    pub studio_only: Vec<String>,
    /// Items that can't be compared, with why.
    pub skipped: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

fn full_path(mapping: &Mapping, key: &str) -> String {
    if key.is_empty() {
        mapping.studio.clone()
    } else {
        format!("{}/{key}", mapping.studio)
    }
}

/// Works out what pushing one mapping's files into Studio does. `resolve` gives the side that wins
/// a conflict, by the instance's full path, when the caller chose one.
pub fn plan_mapping(
    index: usize,
    mapping: &Mapping,
    config: &Config,
    root: &Path,
    studio: StudioSide,
    resolve: &dyn Fn(&str) -> Option<Side>,
) -> Result<MappingPlan> {
    let rules = ModelRule::for_mapping(mapping)?;
    let ignore = config.ignore_set()?;
    let mut plan = MappingPlan {
        studio: mapping.studio.clone(),
        ..MappingPlan::default()
    };

    let mut disk = scan(
        &root.join(&mapping.path),
        &ScanOptions {
            models: &rules,
            ignore: &ignore,
        },
    )?;
    plan.warnings.append(&mut disk.warnings);

    // Studio's side, hashed as the live sync hashes it.
    // Raw, for checking Studio's text didn't change before writing; normalized, for diffs.
    let raw_texts: BTreeMap<String, String> = studio
        .entries
        .iter()
        .filter_map(|e| Some((e.key.clone(), e.source.clone()?)))
        .collect();
    let texts: BTreeMap<String, String> = raw_texts
        .iter()
        .map(|(key, text)| (key.clone(), normalize_text(text)))
        .collect();
    let mut model_hashes = BTreeMap::new();
    for (key, bytes) in &studio.models {
        match modelfile::convert(bytes, rules.format) {
            Ok(converted) => {
                model_hashes.insert(key.clone(), hash_bytes(&converted));
            }
            Err(error) => plan
                .warnings
                .push(format!("couldn't read Studio's {key}: {error:#}")),
        }
    }
    let studio_models = studio.models;
    let mut studio_tree = studio_items(
        studio.entries,
        |text| Ok(hash_bytes(text)),
        |key| model_hashes.get(key).cloned(),
        &mut plan.warnings,
    )?;
    prune_folders(&mut studio_tree);

    let agreed = read_agreed(root, &mapping.id())?;
    let mut keys: Vec<String> = studio_tree
        .keys()
        .chain(disk.items.keys())
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();

    let mut changes: Vec<(String, Option<Item>, &'static str)> = Vec::new();
    for key in keys {
        let s = studio_tree.get(&key).cloned();
        let d = disk.items.get(&key).cloned();
        if s == d {
            continue;
        }
        let path = full_path(mapping, &key);
        if disk.broken.contains(&key) {
            plan.skipped.push((path, "its file can't be read".into()));
            continue;
        }
        if s.as_ref()
            .is_some_and(|i| i.kind == Kind::Model && i.hash.is_none())
        {
            plan.skipped
                .push((path, "Studio couldn't save it to compare".into()));
            continue;
        }
        // Models that differ only in bytes: the same instances, as Studio saves them back.
        if let (Some(si), Some(di)) = (&s, &d) {
            if si.kind == Kind::Model && di.kind == Kind::Model {
                let studio_bytes = studio_models.get(&key);
                let disk_bytes = disk.paths.get(&key).and_then(|p| fs::read(p).ok());
                if let (Some(sb), Some(db)) = (studio_bytes, disk_bytes) {
                    if modeldiff::same_content(sb, &db).unwrap_or(false) {
                        continue;
                    }
                }
            }
        }
        // The same content, and the file simply records no attributes (Script Sync made it):
        // pushing would strip Studio's attributes for nothing.
        if let (Some(si), Some(di)) = (&s, &d) {
            if si.same_shape(di) && si.hash == di.hash && di.meta.is_none() {
                continue;
            }
        }

        let known = agreed.knows(&key, &s);
        let ever_agreed = agreed.base.contains_key(&key) || known;
        let conflict: Option<(&'static str, Option<Side>)> = match (&s, &d) {
            (None, None) => continue,
            (None, Some(_)) => None,
            (Some(_), Some(_)) if known => None,
            (Some(_), Some(_)) if agreed.base.contains_key(&key) => {
                Some(("Studio has changes the files don't", None))
            }
            (Some(_), Some(_)) => Some(("Studio and the file differ, and were never synced", None)),
            (Some(_), None) if known => {
                Some(("Deleted on disk; Studio still has it", Some(Side::Disk)))
            }
            (Some(_), None) if ever_agreed => Some(("Deleted on disk, changed in Studio", None)),
            (Some(_), None) => {
                plan.studio_only.push(path);
                continue;
            }
        };

        if let Some((reason, suggested)) = conflict {
            match resolve(&path) {
                Some(Side::Disk) => {}
                Some(Side::Studio) => {
                    plan.kept.push(path);
                    continue;
                }
                None => {
                    let diff = match (texts.get(&key), disk.sources.get(&key)) {
                        (Some(studio_text), Some(disk_text)) => Some(
                            diffy::DiffOptions::new()
                                .set_original_filename("disk")
                                .set_modified_filename("studio")
                                .create_patch(&normalize_text(disk_text), studio_text)
                                .to_string(),
                        ),
                        _ => None,
                    };
                    plan.conflicts.push(PushConflict {
                        path,
                        reason,
                        suggested,
                        studio: s,
                        disk: d,
                        file: disk.paths.get(&key).cloned(),
                        diff,
                    });
                    continue;
                }
            }
        }
        let change = match (&s, &d) {
            (None, _) => "create",
            (_, None) => "delete",
            _ => "update",
        };
        changes.push((key, d, change));
    }

    // Parents before children; deletes after, children first.
    changes.sort_by_key(|(key, item, _)| {
        let d = depth(key) as i64;
        if item.is_some() {
            (0, d)
        } else {
            (1, -d)
        }
    });
    for (id, (key, target, change)) in changes.into_iter().enumerate() {
        let mut op = Op {
            id: id as u64 + 1,
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
        let mut blob = None;
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
                    op.source = Some(disk.sources.get(&key).cloned().unwrap_or_default());
                }
                Kind::Model => {
                    let path = disk
                        .paths
                        .get(&key)
                        .ok_or_else(|| anyhow::anyhow!("{key} has no file"))?;
                    blob = Some(modelfile::convert(&fs::read(path)?, ModelFormat::Rbxm)?);
                }
                Kind::Folder => {}
            }
        }
        plan.ops.push(PlannedOp {
            path: full_path(mapping, &key),
            studio_source: raw_texts.get(&key).cloned(),
            op,
            change,
            blob,
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::ScriptType;
    use serde_json::json;

    struct Fixture {
        dir: tempfile::TempDir,
        config: Config,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("files")).unwrap();
            let config: Config = serde_json::from_value(json!({
                "mappings": [ { "studio": "ReplicatedStorage/Data", "path": "files", "instances": "listed" } ]
            }))
            .unwrap();
            Fixture { dir, config }
        }

        fn mapping(&self) -> &Mapping {
            &self.config.mappings[0]
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.dir.path().join("files").join(name), text).unwrap();
        }

        /// Records `texts` (by key) as what both sides agreed on, with `past` versions before.
        fn agree(&self, texts: &[(&str, &str)], past: &[(&str, &str)]) {
            let item = |text: &str| json!({ "kind": "script", "scriptType": "module", "hash": hash_bytes(normalize_text(text).as_bytes()) });
            let mut base = serde_json::Map::new();
            base.insert("".into(), json!({ "kind": "folder" }));
            for (key, text) in texts {
                base.insert(key.to_string(), item(text));
            }
            let mut versions = serde_json::Map::new();
            for (key, text) in past {
                versions.insert(
                    key.to_string(),
                    json!([hash_bytes(normalize_text(text).as_bytes())]),
                );
            }
            let id = self.mapping().id();
            let state = json!({ "bases": { id.clone(): base }, "past": { id: versions } });
            fs::create_dir_all(self.dir.path().join(".twoway")).unwrap();
            fs::write(
                self.dir.path().join(".twoway/state.json"),
                state.to_string(),
            )
            .unwrap();
        }

        fn plan(
            &self,
            studio: &[(&str, &str)],
            resolve: &dyn Fn(&str) -> Option<Side>,
        ) -> MappingPlan {
            let mut entries = vec![StudioEntry {
                key: "".into(),
                kind: Kind::Folder,
                script_type: None,
                source: None,
                attributes: None,
                tags: None,
                group: false,
            }];
            for (key, text) in studio {
                entries.push(StudioEntry {
                    key: key.to_string(),
                    kind: Kind::Script,
                    script_type: Some(ScriptType::Module),
                    source: Some(text.to_string()),
                    attributes: None,
                    tags: None,
                    group: false,
                });
            }
            plan_mapping(
                0,
                self.mapping(),
                &self.config,
                self.dir.path(),
                StudioSide {
                    entries,
                    models: BTreeMap::new(),
                },
                resolve,
            )
            .unwrap()
        }
    }

    fn none(_: &str) -> Option<Side> {
        None
    }

    fn changes(plan: &MappingPlan) -> Vec<(String, &'static str)> {
        plan.ops
            .iter()
            .map(|o| (o.op.key.clone(), o.change))
            .collect()
    }

    #[test]
    fn studio_behind_the_agreed_version_gets_the_file() {
        let f = Fixture::new();
        f.write("A.luau", "new");
        f.agree(&[("A", "agreed")], &[]);
        let plan = f.plan(&[("A", "agreed")], &none);
        assert_eq!(changes(&plan), [("A".to_string(), "update")]);
        assert_eq!(plan.ops[0].op.source.as_deref(), Some("new"));
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn an_old_version_from_before_counts_as_behind() {
        let f = Fixture::new();
        f.write("A.luau", "new");
        f.agree(&[("A", "new")], &[("A", "old")]);
        let plan = f.plan(&[("A", "old")], &none);
        assert_eq!(changes(&plan), [("A".to_string(), "update")]);
    }

    #[test]
    fn studio_work_the_sync_never_saw_is_a_conflict_and_left_alone() {
        let f = Fixture::new();
        f.write("A.luau", "new");
        f.agree(&[("A", "agreed")], &[]);
        let plan = f.plan(&[("A", "typed in studio")], &none);
        assert!(plan.ops.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, "ReplicatedStorage/Data/A");
        assert!(plan.conflicts[0]
            .diff
            .as_deref()
            .unwrap()
            .contains("+typed in studio"));
    }

    #[test]
    fn a_chosen_side_settles_a_conflict() {
        let f = Fixture::new();
        f.write("A.luau", "new");
        f.agree(&[("A", "agreed")], &[]);
        let disk = f.plan(&[("A", "typed")], &|_| Some(Side::Disk));
        assert_eq!(changes(&disk), [("A".to_string(), "update")]);
        let studio = f.plan(&[("A", "typed")], &|_| Some(Side::Studio));
        assert!(studio.ops.is_empty() && studio.conflicts.is_empty());
        assert_eq!(studio.kept, ["ReplicatedStorage/Data/A"]);
    }

    #[test]
    fn missing_instances_are_created() {
        let f = Fixture::new();
        f.write("A.luau", "a");
        f.agree(&[], &[]);
        let plan = f.plan(&[], &none);
        assert_eq!(changes(&plan), [("A".to_string(), "create")]);
    }

    #[test]
    fn a_file_deleted_on_disk_asks_with_disk_suggested() {
        let f = Fixture::new();
        f.agree(&[("A", "a")], &[]);
        let plan = f.plan(&[("A", "a")], &none);
        assert!(plan.ops.is_empty());
        assert_eq!(plan.conflicts[0].suggested, Some(Side::Disk));
        let resolved = f.plan(&[("A", "a")], &|_| Some(Side::Disk));
        assert_eq!(changes(&resolved), [("A".to_string(), "delete")]);
    }

    #[test]
    fn instances_only_studio_ever_had_are_left_alone() {
        let f = Fixture::new();
        f.agree(&[], &[]);
        let plan = f.plan(&[("Mine", "x")], &none);
        assert!(plan.ops.is_empty() && plan.conflicts.is_empty());
        assert_eq!(plan.studio_only, ["ReplicatedStorage/Data/Mine"]);
    }

    #[test]
    fn nothing_is_written_to_the_files_or_the_state() {
        let f = Fixture::new();
        f.write("A.luau", "new");
        f.agree(&[("A", "agreed")], &[]);
        let state = fs::read(f.dir.path().join(".twoway/state.json")).unwrap();
        f.plan(&[("A", "typed")], &|_| Some(Side::Disk));
        assert_eq!(
            fs::read(f.dir.path().join(".twoway/state.json")).unwrap(),
            state
        );
        assert_eq!(
            fs::read_to_string(f.dir.path().join("files/A.luau")).unwrap(),
            "new"
        );
        assert!(!f.dir.path().join(".twoway/blobs").exists());
    }
}

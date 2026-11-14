//! The file layout: how Studio instances and files map to each other, and reading a folder.
//!
//! New files follow Rojo: `Name.luau` is a ModuleScript, `Name.server.luau` a Script,
//! `Name.client.luau` a LocalScript and `Name.plugin.luau` a plugin Script. A directory is a
//! Folder, unless it holds `init.*`, which makes it that script with children.
//!
//! Files written by Studio Script Sync keep working: `.local` reads as client and `.legacy` as
//! server. Nothing renames them; only a new file gets the Rojo name. Script Sync's `.client` means
//! a Script with RunContext Client, which is the same family as a LocalScript, so it matches too.
//!
//! Every synced item has a key: instance names joined with `/`, relative to the mapping's root.
//! The root itself is the empty key.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::config::{InstanceMode, Mapping};
use crate::modelfile::{model_hash, parse_model_file, ModelFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptType {
    Module,
    Server,
    Client,
    Legacy,
    Plugin,
    Local,
}

impl ScriptType {
    /// What a file name can say. Rojo has no `local` or `legacy`; those are Script Sync's.
    const SUFFIXES: [(&'static str, ScriptType); 5] = [
        ("server", ScriptType::Server),
        ("client", ScriptType::Client),
        ("legacy", ScriptType::Legacy),
        ("plugin", ScriptType::Plugin),
        ("local", ScriptType::Local),
    ];

    /// The suffix a new file gets, as Rojo names it.
    pub fn suffix(self) -> Option<&'static str> {
        match self.family() {
            ScriptType::Server => Some("server"),
            ScriptType::Client => Some("client"),
            ScriptType::Plugin => Some("plugin"),
            _ => None,
        }
    }

    /// Types that mean the same thing compare equal through this.
    pub fn family(self) -> ScriptType {
        match self {
            ScriptType::Legacy => ScriptType::Server,
            ScriptType::Local => ScriptType::Client,
            other => other,
        }
    }

    /// The class a new instance gets, as in Rojo with its default `emitLegacyScripts`.
    pub fn class_name(self) -> &'static str {
        match self.family() {
            ScriptType::Module => "ModuleScript",
            ScriptType::Client => "LocalScript",
            _ => "Script",
        }
    }

    /// The `Enum.RunContext` value of a new `Script`.
    pub fn run_context(self) -> Option<u32> {
        match self.family() {
            ScriptType::Server => Some(0),
            ScriptType::Plugin => Some(3),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Folder,
    Script,
    Model,
}

/// What an item is, on any side. Two sides agree when their `Item`s are equal.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_type: Option<ScriptType>,
    /// Hash of the content. Folders have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Attributes and tags, as `canonical_meta` writes them. Models keep theirs inside the `.rbxm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.same_shape(other) && self.hash == other.hash && self.meta == other.meta
    }
}

impl Item {
    /// The same kind of thing, whatever it holds.
    pub fn same_shape(&self, other: &Item) -> bool {
        self.kind == other.kind
            && self.script_type.map(ScriptType::family) == other.script_type.map(ScriptType::family)
    }

    pub fn folder() -> Self {
        Item {
            kind: Kind::Folder,
            script_type: None,
            hash: None,
            meta: None,
        }
    }
}

pub type Tree = BTreeMap<String, Item>;

/// Hashes text the way both sides are compared. Line endings don't count as a change.
pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn join_key(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

pub fn parent_key(key: &str) -> Option<&str> {
    if key.is_empty() {
        return None;
    }
    Some(key.rsplit_once('/').map_or("", |(parent, _)| parent))
}

pub fn depth(key: &str) -> usize {
    if key.is_empty() {
        0
    } else {
        key.matches('/').count() + 1
    }
}

/// Why this instance name can't be a file name, if it can't.
pub fn name_problem(name: &str) -> Option<&'static str> {
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if name.is_empty() {
        return Some("it is empty");
    }
    if name
        .chars()
        .any(|c| "/\\:*?\"<>|".contains(c) || c.is_control())
    {
        return Some("it has a character files can't use");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("it ends with a dot or a space");
    }
    if name.eq_ignore_ascii_case("init") {
        return Some("`init` is the file name of its parent script");
    }
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Some("Windows reserves that name");
    }
    None
}

/// Splits `Name.server.luau` into `Name` and its type. None for anything that isn't a script file.
pub fn parse_script_file(file_name: &str) -> Option<(&str, ScriptType)> {
    let stem = file_name
        .strip_suffix(".luau")
        .or_else(|| file_name.strip_suffix(".lua"))?;
    for (suffix, script_type) in ScriptType::SUFFIXES {
        if let Some(name) = stem.strip_suffix(suffix).and_then(|s| s.strip_suffix('.')) {
            return Some((name, script_type));
        }
    }
    Some((stem, ScriptType::Module))
}

pub fn script_file_name(name: &str, script_type: ScriptType) -> String {
    match script_type.suffix() {
        Some(suffix) => format!("{name}.{suffix}.luau"),
        None => format!("{name}.luau"),
    }
}

/// A folder as read from disk.
#[derive(Debug, Default)]
pub struct DiskTree {
    pub items: Tree,
    /// Where each key lives: the file, or the directory for a Folder.
    pub paths: BTreeMap<String, PathBuf>,
    /// Text of each script, by key.
    pub sources: BTreeMap<String, String>,
    /// Model files that can't be read. They're left out of the sync, on both sides, until fixed:
    /// treating them as missing would delete the instance in Studio.
    pub broken: BTreeSet<String>,
    pub warnings: Vec<String>,
}

/// The instances saved as one `.rbxm` each. An entry can use `*` for any one name, as in
/// `Biomes/*/Sounds`, or `**` for any depth.
#[derive(Debug, Clone, Default)]
pub struct ModelSet {
    exact: BTreeSet<String>,
    patterns: GlobSet,
}

impl ModelSet {
    pub fn new(list: &[String]) -> Result<Self> {
        let mut exact = BTreeSet::new();
        let mut patterns = GlobSetBuilder::new();
        for entry in list {
            if entry.contains(['*', '?', '[']) {
                patterns.add(GlobBuilder::new(entry).literal_separator(true).build()?);
            } else {
                exact.insert(entry.clone());
            }
        }
        Ok(Self {
            exact,
            patterns: patterns.build()?,
        })
    }

    pub fn contains(&self, key: &str) -> bool {
        self.exact.contains(key) || self.patterns.is_match(key)
    }
}

/// Which instances of a mapping are saved as model files, and how.
#[derive(Debug, Clone, Default)]
pub struct ModelRule {
    listed: ModelSet,
    exclude: ModelSet,
    /// Every instance that isn't a script or a Folder is a model.
    pub auto: bool,
    /// In auto mode, a folder's loose instances share one `init` file, unless the folder is
    /// listed in `separate`.
    pub group: bool,
    separate: ModelSet,
    pub format: ModelFormat,
}

/// The key of a folder's shared file of loose instances: `<folder>/init`. No instance can have it,
/// since `init` is never a synced name.
pub const GROUP_NAME: &str = "init";

pub fn is_group_key(key: &str) -> bool {
    key.rsplit('/').next() == Some(GROUP_NAME)
}

pub fn group_key(container: &str) -> String {
    join_key(container, GROUP_NAME)
}

/// What an instance under a mapped root is to the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Saved as one model file, everything inside it included.
    Model,
    /// Saved with the other loose instances of its folder, in the folder's `init` file.
    Grouped,
    /// A script or Folder: synced, and what's inside is looked at too.
    Container,
    /// Not ours: never read, written or deleted.
    Skipped,
}

impl ModelRule {
    pub fn for_mapping(mapping: &Mapping) -> Result<Self> {
        Self::new(
            &mapping.models,
            &mapping.exclude,
            mapping.instances == InstanceMode::Auto,
            mapping.model_format,
        )?
        .with_groups(mapping.group_instances, &mapping.separate)
    }

    /// Without groups: every model has its own file.
    pub fn new(
        models: &[String],
        exclude: &[String],
        auto: bool,
        format: ModelFormat,
    ) -> Result<Self> {
        Ok(Self {
            listed: ModelSet::new(models)?,
            exclude: ModelSet::new(exclude)?,
            auto,
            group: false,
            separate: ModelSet::default(),
            format,
        })
    }

    pub fn with_groups(mut self, group: bool, separate: &[String]) -> Result<Self> {
        self.group = group;
        self.separate = ModelSet::new(separate)?;
        Ok(self)
    }

    /// The loose instances of this script or folder share one file.
    pub fn groups_under(&self, container: &str) -> bool {
        self.auto && self.group && !self.separate.contains(container)
    }

    /// Only the listed instances, as binary.
    #[cfg(test)]
    pub fn listed(models: &[&str]) -> Self {
        let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();
        Self::new(&models, &[], false, ModelFormat::Rbxm).expect("valid patterns")
    }

    pub fn excluded(&self, key: &str) -> bool {
        self.exclude.contains(key)
    }

    pub fn is_listed(&self, key: &str) -> bool {
        !self.excluded(key) && self.listed.contains(key)
    }

    /// A model file found on disk at this key syncs. Under a folder that groups its instances,
    /// only the `init` file does (and listed models); under one that doesn't, only single files.
    pub fn takes_file(&self, key: &str) -> bool {
        let parent = parent_key(key).unwrap_or("");
        if is_group_key(key) {
            return self.groups_under(parent) && !self.excluded(parent);
        }
        if self.excluded(key) {
            return false;
        }
        self.listed.contains(key) || (self.auto && !self.groups_under(parent))
    }

    /// `container` is true for a script or a Folder.
    pub fn role(&self, key: &str, container: bool) -> Role {
        if self.excluded(key) {
            Role::Skipped
        } else if self.listed.contains(key) {
            Role::Model
        } else if container {
            Role::Container
        } else if self.groups_under(parent_key(key).unwrap_or("")) {
            Role::Grouped
        } else if self.auto {
            Role::Model
        } else {
            Role::Skipped
        }
    }
}

pub struct ScanOptions<'a> {
    pub models: &'a ModelRule,
    pub ignore: &'a GlobSet,
}

pub fn scan(root: &Path, options: &ScanOptions) -> Result<DiskTree> {
    let mut tree = DiskTree::default();
    if root.is_dir() {
        scan_dir(root, root, "", options, &mut tree)?;
    }
    prune_folders(&mut tree.items);
    Ok(tree)
}

/// A folder syncs only when it has something to sync: a script or model somewhere inside, or
/// attributes or tags of its own. One holding only other instances (a folder of Sounds) doesn't
/// become an empty directory, and a directory holding only notes doesn't become a Folder.
pub fn prune_folders(items: &mut Tree) {
    let mut keep: BTreeSet<String> = BTreeSet::from([String::new()]);
    for (key, item) in items.iter() {
        if item.kind != Kind::Folder || item.meta.is_some() {
            let mut current = Some(key.as_str());
            while let Some(k) = current {
                if !keep.insert(k.to_string()) {
                    break;
                }
                current = parent_key(k);
            }
        }
    }
    items.retain(|key, item| item.kind != Kind::Folder || keep.contains(key));
}

fn read_script(path: &Path, tree: &mut DiskTree, key: &str, script_type: ScriptType) -> Result<()> {
    let bytes = fs::read(path)?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => normalize_text(&text),
        Err(_) => {
            tree.warnings
                .push(format!("{} is not UTF-8, skipped", path.display()));
            return Ok(());
        }
    };
    tree.items.insert(
        key.to_string(),
        Item {
            kind: Kind::Script,
            script_type: Some(script_type),
            hash: Some(hash_bytes(text.as_bytes())),
            meta: None,
        },
    );
    tree.paths.insert(key.to_string(), path.to_path_buf());
    tree.sources.insert(key.to_string(), text);
    read_meta(tree, key);
    Ok(())
}

/// Reads the attributes and tags of an item already in the tree.
fn read_meta(tree: &mut DiskTree, key: &str) {
    let (Some(path), Some(item)) = (tree.paths.get(key), tree.items.get(key)) else {
        return;
    };
    let Some(meta_file) = meta_path(path, item.kind) else {
        return;
    };
    let Ok(text) = fs::read_to_string(&meta_file) else {
        return;
    };
    match parse_meta(&text) {
        Ok(meta) => {
            if let Some(item) = tree.items.get_mut(key) {
                item.meta = meta;
            }
        }
        Err(error) => {
            tree.warnings.push(format!(
                "{} isn't valid JSON, so its attributes aren't synced: {error}",
                meta_file.display()
            ));
        }
    }
}

/// Where an item's attributes and tags live, as in Rojo: `Name.meta.json` beside a script file, and
/// `init.meta.json` in the directory of a folder or of a script with children.
pub fn meta_path(content: &Path, kind: Kind) -> Option<PathBuf> {
    match kind {
        Kind::Folder => Some(content.join("init.meta.json")),
        Kind::Script => {
            let file_name = content.file_name()?.to_str()?;
            let (name, _) = parse_script_file(file_name)?;
            Some(content.with_file_name(format!("{name}.meta.json")))
        }
        Kind::Model => None,
    }
}

/// Numbers inside a typed value (`Vector3`, `Color3`, ...) are 32-bit in Roblox, so they're
/// compared at that precision: Studio's `0.10000000149011612` and a file's `0.1` are the same.
/// Whole numbers are written without a fraction, which `UDim` offsets need.
fn normalize(value: &Value, nested: bool) -> Value {
    match value {
        Value::Number(number) => {
            let Some(mut x) = number.as_f64() else {
                return value.clone();
            };
            if nested {
                if let Ok(rounded) = format!("{}", x as f32).parse::<f64>() {
                    x = rounded;
                }
            }
            if x.fract() == 0.0 && x.abs() < 9.0e15 {
                Value::from(x as i64)
            } else {
                Number::from_f64(x).map_or(Value::Null, Value::Number)
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| normalize(v, true)).collect()),
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, normalize(v, true))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        other => other.clone(),
    }
}

/// One text for an item's attributes and tags, the same whichever side it came from. None when
/// there are neither.
pub fn canonical_meta(attributes: Option<&Value>, tags: Option<&[String]>) -> Option<String> {
    let mut out = Map::new();
    if let Some(Value::Object(map)) = attributes {
        if !map.is_empty() {
            out.insert(
                "attributes".into(),
                normalize(&Value::Object(map.clone()), false),
            );
            // normalize() treats the object's own values as nested; plain numbers are 64-bit.
            if let Some(Value::Object(normalized)) = out.get_mut("attributes") {
                for (name, value) in map {
                    if value.is_number() {
                        normalized.insert(name.clone(), normalize(value, false));
                    }
                }
            }
        }
    }
    let mut list: Vec<String> = tags.unwrap_or(&[]).to_vec();
    list.sort();
    list.dedup();
    if !list.is_empty() {
        out.insert("tags".into(), Value::from(list));
    }
    (!out.is_empty()).then(|| Value::Object(out).to_string())
}

/// The attributes and tags in a Rojo meta file.
pub fn parse_meta(text: &str) -> Result<Option<String>> {
    let value: Value = serde_json::from_str(text)?;
    let tags: Option<Vec<String>> = value
        .get("properties")
        .and_then(|p| p.get("Tags"))
        .and_then(|t| serde_json::from_value(t.clone()).ok());
    Ok(canonical_meta(value.get("attributes"), tags.as_deref()))
}

/// The attributes and tags as separate values, for the plugin and for `build`.
pub fn split_meta(meta: Option<&str>) -> (Map<String, Value>, Vec<String>) {
    let value: Value = meta
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or(Value::Null);
    let attributes = value
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let tags = value
        .get("tags")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    (attributes, tags)
}

/// Writes attributes and tags into a Rojo meta file, keeping anything else in it. The file goes
/// when nothing is left in it. A file that isn't a JSON object is never overwritten.
pub fn write_meta(path: &Path, meta: Option<&str>) -> Result<()> {
    let mut object = match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(object)) => {
                if parse_meta(&text)?.as_deref() == meta {
                    return Ok(());
                }
                object
            }
            _ => anyhow::bail!(
                "{} isn't a JSON object, so it was left alone",
                path.display()
            ),
        },
        Err(_) if meta.is_none() => return Ok(()),
        Err(_) => Map::new(),
    };

    object.remove("attributes");
    let mut properties = match object.remove("properties") {
        Some(Value::Object(properties)) => properties,
        _ => Map::new(),
    };
    properties.remove("Tags");
    let (attributes, tags) = split_meta(meta);
    if !attributes.is_empty() {
        object.insert("attributes".into(), Value::Object(attributes));
    }
    if !tags.is_empty() {
        properties.insert("Tags".into(), Value::from(tags));
    }
    if !properties.is_empty() {
        object.insert("properties".into(), Value::Object(properties));
    }

    if object.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&Value::Object(object))? + "\n",
    )?;
    Ok(())
}

fn scan_dir(
    root: &Path,
    dir: &Path,
    key: &str,
    options: &ScanOptions,
    tree: &mut DiskTree,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    // `init.*` decides what the directory itself is.
    let mut is_script = false;
    for entry in &entries {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if let Some(("init", script_type)) = parse_script_file(&file_name) {
            if entry.file_type()?.is_file() {
                read_script(&entry.path(), tree, key, script_type)?;
                is_script = true;
                break;
            }
        }
    }
    if !is_script {
        tree.items.insert(key.to_string(), Item::folder());
        tree.paths.insert(key.to_string(), dir.to_path_buf());
        read_meta(tree, key);
    }

    for entry in &entries {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if file_name == ".twoway" || options.ignore.is_match(relative) {
            continue;
        }

        if entry.file_type()?.is_dir() {
            let child = join_key(key, &file_name);
            if options.models.excluded(&child) {
                continue;
            }
            if options.models.is_listed(&child) {
                tree.warnings
                    .push(format!("{child} is listed in models, but is a directory"));
                continue;
            }
            scan_dir(root, &path, &child, options, tree)?;
        } else if let Some((name, format)) = parse_model_file(&file_name) {
            let child = join_key(key, name);
            if !options.models.takes_file(&child) {
                if options.models.groups_under(key) && !options.models.excluded(&child) {
                    tree.warnings.push(format!(
                        "{} isn't synced: this folder keeps its instances in {GROUP_NAME}.{}",
                        path.display(),
                        options.models.format.ext()
                    ));
                }
                continue;
            }
            if let Some(other) = tree.paths.get(&child) {
                tree.warnings.push(format!(
                    "{} and {} are the same instance; delete one",
                    other.display(),
                    path.display()
                ));
                // The one in the mapping's format wins.
                if format != options.models.format {
                    continue;
                }
            }
            match model_hash(&fs::read(&path)?, options.models.format) {
                Ok(hash) => {
                    tree.broken.remove(&child);
                    tree.items.insert(
                        child.clone(),
                        Item {
                            kind: Kind::Model,
                            script_type: None,
                            hash: Some(hash),
                            meta: None,
                        },
                    );
                    tree.paths.insert(child, path);
                }
                Err(error) => {
                    tree.warnings.push(format!(
                        "{} can't be read, so it isn't synced until fixed: {error:#}",
                        path.display()
                    ));
                    tree.broken.insert(child);
                }
            }
        } else if let Some((name, script_type)) = parse_script_file(&file_name) {
            let child = join_key(key, name);
            if name != "init" && !options.models.excluded(&child) {
                read_script(&path, tree, &child, script_type)?;
            }
        }
    }
    Ok(())
}

/// Where a new item goes, under `root`.
pub fn default_path(root: &Path, key: &str, item: &Item, has_children: bool) -> PathBuf {
    let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
    let dir = key_dir(root, parent);
    match item.kind {
        Kind::Folder => key_dir(root, key),
        Kind::Model => dir.join(format!("{name}.rbxm")),
        Kind::Script => {
            let script_type = item.script_type.unwrap_or(ScriptType::Module);
            if key.is_empty() || has_children {
                key_dir(root, key).join(script_file_name("init", script_type))
            } else {
                dir.join(script_file_name(name, script_type))
            }
        }
    }
}

/// Where a model file goes, in the mapping's format.
pub fn model_path(root: &Path, key: &str, format: ModelFormat) -> PathBuf {
    let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
    key_dir(root, parent).join(format!("{name}.{}", format.ext()))
}

pub fn key_dir(root: &Path, key: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    if !key.is_empty() {
        path.extend(key.split('/'));
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::GlobSetBuilder;

    #[test]
    fn script_names_round_trip() {
        for (file, name, script_type) in [
            ("Foo.luau", "Foo", ScriptType::Module),
            ("Foo.server.luau", "Foo", ScriptType::Server),
            ("Foo.client.luau", "Foo", ScriptType::Client),
            ("Foo.plugin.luau", "Foo", ScriptType::Plugin),
            ("a.b.luau", "a.b", ScriptType::Module),
        ] {
            assert_eq!(parse_script_file(file), Some((name, script_type)));
            assert_eq!(script_file_name(name, script_type), file);
        }
        assert_eq!(
            parse_script_file("Foo.lua"),
            Some(("Foo", ScriptType::Module))
        );
        assert_eq!(parse_script_file("notes.md"), None);
    }

    #[test]
    fn script_sync_names_read_but_new_files_use_rojo_names() {
        assert_eq!(
            parse_script_file("Foo.local.luau"),
            Some(("Foo", ScriptType::Local))
        );
        assert_eq!(
            parse_script_file("Foo.legacy.luau"),
            Some(("Foo", ScriptType::Legacy))
        );
        assert_eq!(
            script_file_name("Foo", ScriptType::Local),
            "Foo.client.luau"
        );
        assert_eq!(
            script_file_name("Foo", ScriptType::Legacy),
            "Foo.server.luau"
        );
    }

    #[test]
    fn new_instances_match_rojo() {
        assert_eq!(ScriptType::Client.class_name(), "LocalScript");
        assert_eq!(ScriptType::Local.class_name(), "LocalScript");
        assert_eq!(ScriptType::Server.class_name(), "Script");
        assert_eq!(ScriptType::Server.run_context(), Some(0), "Legacy");
        assert_eq!(ScriptType::Legacy.run_context(), Some(0));
        assert_eq!(ScriptType::Plugin.run_context(), Some(3));
    }

    #[test]
    fn script_sync_and_rojo_types_compare_equal() {
        let item = |t| Item {
            kind: Kind::Script,
            script_type: Some(t),
            hash: Some("h".into()),
            meta: None,
        };
        assert_eq!(item(ScriptType::Local), item(ScriptType::Client));
        assert_eq!(item(ScriptType::Legacy), item(ScriptType::Server));
        assert_ne!(item(ScriptType::Client), item(ScriptType::Server));
    }

    #[test]
    fn bad_names_are_caught() {
        for name in ["a/b", "a:b", "", "x.", "init", "INIT", "con"] {
            assert!(name_problem(name).is_some(), "{name}");
        }
        assert!(name_problem("Fine Name").is_none());
    }

    #[test]
    fn scan_reads_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("Kit/Sub")).unwrap();
        fs::create_dir_all(root.join("Scripted")).unwrap();
        fs::write(root.join("Kit/Mod.luau"), "return 1\r\n").unwrap();
        fs::write(root.join("Kit/Run.client.luau"), "print(1)").unwrap();
        fs::write(root.join("Kit/notes.md"), "ignored, not a script").unwrap();
        fs::write(
            root.join("Kit/Other.rbxm"),
            crate::modelfile::tests::sample(),
        )
        .unwrap();
        fs::write(
            root.join("Kit/Assets.rbxm"),
            crate::modelfile::tests::sample(),
        )
        .unwrap();
        fs::write(root.join("Scripted/init.local.luau"), "x").unwrap();
        fs::write(root.join("Scripted/Child.luau"), "y").unwrap();

        let models = ModelRule::listed(&["Kit/Assets"]);
        let ignore = GlobSetBuilder::new().build().unwrap();
        let tree = scan(
            root,
            &ScanOptions {
                models: &models,
                ignore: &ignore,
            },
        )
        .unwrap();

        let keys: Vec<_> = tree.items.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "",
                "Kit",
                "Kit/Assets",
                "Kit/Mod",
                "Kit/Run",
                "Scripted",
                "Scripted/Child"
            ]
        );
        assert_eq!(tree.items["Kit"].kind, Kind::Folder);
        assert_eq!(tree.items["Kit/Assets"].kind, Kind::Model);
        assert_eq!(tree.items["Kit/Run"].script_type, Some(ScriptType::Client));
        assert_eq!(tree.items["Scripted"].script_type, Some(ScriptType::Local));
        assert_eq!(tree.sources["Kit/Mod"], "return 1\n", "CRLF is normalized");
    }

    #[test]
    fn meta_files_sit_where_rojo_puts_them() {
        assert_eq!(
            meta_path(Path::new("r/A.client.luau"), Kind::Script),
            Some(PathBuf::from("r/A.meta.json"))
        );
        assert_eq!(
            meta_path(Path::new("r/A/init.luau"), Kind::Script),
            Some(PathBuf::from("r/A/init.meta.json"))
        );
        assert_eq!(
            meta_path(Path::new("r/A"), Kind::Folder),
            Some(PathBuf::from("r/A/init.meta.json"))
        );
        assert_eq!(meta_path(Path::new("r/A.rbxm"), Kind::Model), None);
    }

    #[test]
    fn studio_precision_and_file_precision_compare_equal() {
        let studio: Value = serde_json::from_str(
            r#"{ "Tint": {"Color3": [0.10000000149011612, 0.5, 1]}, "Pad": {"UDim": [0.5, 10]}, "Speed": 0.1 }"#,
        )
        .unwrap();
        let file: Value = serde_json::from_str(
            r#"{ "Pad": {"UDim": [0.5, 10.0]}, "Speed": 0.1, "Tint": {"Color3": [0.1, 0.5, 1.0]} }"#,
        )
        .unwrap();
        assert_eq!(
            canonical_meta(Some(&studio), None),
            canonical_meta(Some(&file), None)
        );
        let text = canonical_meta(Some(&studio), None).unwrap();
        assert!(
            text.contains(r#""UDim":[0.5,10]"#),
            "offsets stay whole numbers: {text}"
        );
    }

    #[test]
    fn a_plain_number_keeps_its_full_precision() {
        let a: Value = serde_json::from_str(r#"{ "Chance": 0.123456789012 }"#).unwrap();
        assert!(canonical_meta(Some(&a), None)
            .unwrap()
            .contains("0.123456789012"));
    }

    #[test]
    fn tags_are_sorted_and_empty_meta_is_none() {
        let tags = vec!["b".to_string(), "a".to_string(), "a".to_string()];
        assert_eq!(
            canonical_meta(None, Some(&tags)).unwrap(),
            r#"{"tags":["a","b"]}"#
        );
        assert_eq!(
            canonical_meta(Some(&Value::Object(Map::new())), Some(&[])),
            None
        );
        assert_eq!(
            canonical_meta(Some(&Value::Array(vec![])), None),
            None,
            "Luau sends an empty table as []"
        );
    }

    #[test]
    fn write_meta_keeps_other_keys_and_removes_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("A.meta.json");
        fs::write(
            &path,
            r#"{ "ignoreUnknownInstances": true, "properties": { "Disabled": true } }"#,
        )
        .unwrap();

        let meta = canonical_meta(
            Some(&serde_json::json!({ "Weight": 3 })),
            Some(&["Tagged".to_string()]),
        );
        write_meta(&path, meta.as_deref()).unwrap();
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["ignoreUnknownInstances"], true);
        assert_eq!(written["properties"]["Disabled"], true);
        assert_eq!(written["properties"]["Tags"], serde_json::json!(["Tagged"]));
        assert_eq!(written["attributes"]["Weight"], 3);
        assert_eq!(
            parse_meta(&fs::read_to_string(&path).unwrap()).unwrap(),
            meta
        );

        write_meta(&path, None).unwrap();
        let left: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            left,
            serde_json::json!({ "ignoreUnknownInstances": true, "properties": { "Disabled": true } })
        );

        let only = dir.path().join("B.meta.json");
        write_meta(&only, meta.as_deref()).unwrap();
        write_meta(&only, None).unwrap();
        assert!(!only.exists(), "nothing left, so the file goes");
    }

    #[test]
    fn write_meta_never_overwrites_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("A.meta.json");
        fs::write(&path, "{ not json").unwrap();
        let meta = canonical_meta(None, Some(&["x".to_string()]));
        assert!(write_meta(&path, meta.as_deref()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn scan_reads_meta_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("Dir")).unwrap();
        fs::write(
            root.join("Dir/init.meta.json"),
            r#"{ "attributes": { "On": true } }"#,
        )
        .unwrap();
        fs::write(root.join("A.server.luau"), "x").unwrap();
        fs::write(
            root.join("A.meta.json"),
            r#"{ "properties": { "Tags": ["T"] } }"#,
        )
        .unwrap();
        let models = ModelRule::default();
        let ignore = globset::GlobSetBuilder::new().build().unwrap();
        let tree = scan(
            root,
            &ScanOptions {
                models: &models,
                ignore: &ignore,
            },
        )
        .unwrap();
        assert_eq!(
            tree.items["Dir"].meta.as_deref(),
            Some(r#"{"attributes":{"On":true}}"#)
        );
        assert_eq!(tree.items["A"].meta.as_deref(), Some(r#"{"tags":["T"]}"#));
        assert!(
            !tree.items.contains_key("A.meta"),
            "a meta file is not an item"
        );
    }

    #[test]
    fn auto_mode_takes_every_model_file_but_the_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sample = crate::modelfile::tests::sample();
        let xml = crate::modelfile::convert(&sample, ModelFormat::Rbxmx).unwrap();
        fs::create_dir_all(root.join("Preset")).unwrap();
        fs::create_dir_all(root.join("Huge")).unwrap();
        fs::write(root.join("Preset/Lighting.luau"), "return {}").unwrap();
        fs::write(root.join("Preset/Sky.rbxm"), &sample).unwrap();
        fs::write(root.join("Preset/Air.rbxmx"), &xml).unwrap();
        fs::write(root.join("Preset/Broken.rbxmx"), "<roblox><Item").unwrap();
        fs::write(root.join("Huge/Map.rbxm"), &sample).unwrap();
        fs::write(root.join("Skip.luau"), "x").unwrap();

        let exclude = vec!["Huge".to_string(), "Skip".to_string()];
        let models = ModelRule::new(&[], &exclude, true, ModelFormat::Rbxmx).unwrap();
        let ignore = GlobSetBuilder::new().build().unwrap();
        let tree = scan(
            root,
            &ScanOptions {
                models: &models,
                ignore: &ignore,
            },
        )
        .unwrap();

        let keys: Vec<_> = tree.items.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["", "Preset", "Preset/Air", "Preset/Lighting", "Preset/Sky"]
        );
        // Both compare in the mapping's format, whatever their own.
        assert_eq!(tree.items["Preset/Sky"].hash, tree.items["Preset/Air"].hash);
        assert!(tree.broken.contains("Preset/Broken"));
        assert!(tree.warnings.iter().any(|w| w.contains("Broken.rbxmx")));
    }

    #[test]
    fn a_grouping_folder_keeps_its_instances_in_init() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let xml = crate::modelfile::convert(
            &crate::modelfile::tests::sample_group(0.3),
            ModelFormat::Rbxmx,
        )
        .unwrap();
        fs::create_dir_all(root.join("Preset")).unwrap();
        fs::create_dir_all(root.join("Separate")).unwrap();
        fs::write(root.join("Preset/Data.luau"), "return 1").unwrap();
        fs::write(root.join("Preset/init.rbxmx"), &xml).unwrap();
        fs::write(root.join("Preset/Sky.rbxmx"), &xml).unwrap();
        fs::write(root.join("Separate/Sky.rbxmx"), &xml).unwrap();
        fs::write(root.join("Separate/init.rbxmx"), &xml).unwrap();

        let models = ModelRule::new(&["Preset/Pack".into()], &[], true, ModelFormat::Rbxmx)
            .unwrap()
            .with_groups(true, &["Separate".into()])
            .unwrap();
        let ignore = GlobSetBuilder::new().build().unwrap();
        let tree = scan(
            root,
            &ScanOptions {
                models: &models,
                ignore: &ignore,
            },
        )
        .unwrap();

        let keys: Vec<_> = tree.items.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "",
                "Preset",
                "Preset/Data",
                "Preset/init",
                "Separate",
                "Separate/Sky"
            ]
        );
        assert!(
            tree.warnings
                .iter()
                .any(|w| w.contains("Sky.rbxmx") && w.contains("init.rbxmx")),
            "{:?}",
            tree.warnings
        );
        assert_eq!(models.role("Preset/Sky", false), Role::Grouped);
        assert_eq!(
            models.role("Preset/Pack", false),
            Role::Model,
            "a listed instance keeps its own file"
        );
        assert_eq!(models.role("Separate/Sky", false), Role::Model);
        assert_eq!(models.role("Preset/Folder", true), Role::Container);
        assert!(is_group_key("init") && is_group_key("A/init") && !is_group_key("A/initial"));
    }

    #[test]
    fn roles_follow_the_rule() {
        let rule = ModelRule::new(
            &["Pack".into()],
            &["Secret".into()],
            true,
            ModelFormat::Rbxm,
        )
        .unwrap();
        assert_eq!(
            rule.role("Pack", true),
            Role::Model,
            "a listed script or folder is packed whole"
        );
        assert_eq!(rule.role("Dir", true), Role::Container);
        assert_eq!(rule.role("Part", false), Role::Model);
        assert_eq!(rule.role("Secret", false), Role::Skipped);
        let listed = ModelRule::listed(&["Pack"]);
        assert_eq!(listed.role("Part", false), Role::Skipped);
        assert_eq!(
            model_path(Path::new("r"), "A/B", ModelFormat::Rbxmx),
            Path::new("r/A/B.rbxmx")
        );
    }

    #[test]
    fn model_patterns_match_one_name_per_star() {
        let models = ModelSet::new(&["Biomes/*/Sounds".to_string(), "Exact".to_string()]).unwrap();
        assert!(models.contains("Biomes/Base/Sounds"));
        assert!(models.contains("Exact"));
        assert!(!models.contains("Biomes/Base/Deeper/Sounds"));
        assert!(!models.contains("Biomes/Base"));
    }

    #[test]
    fn only_folders_with_something_to_sync_are_kept() {
        let folder = |meta: Option<&str>| Item {
            meta: meta.map(str::to_string),
            ..Item::folder()
        };
        let script = Item {
            kind: Kind::Script,
            script_type: Some(ScriptType::Module),
            hash: Some("h".into()),
            meta: None,
        };
        let mut items = Tree::from([
            (String::new(), folder(None)),
            ("Empty".to_string(), folder(None)),
            ("Sounds".to_string(), folder(None)),
            ("Colors".to_string(), folder(None)),
            ("Colors/Common".to_string(), folder(Some("{}"))),
            ("Code".to_string(), folder(None)),
            ("Code/Deep".to_string(), folder(None)),
            ("Code/Deep/A".to_string(), script),
        ]);
        prune_folders(&mut items);
        let keys: Vec<_> = items.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "",
                "Code",
                "Code/Deep",
                "Code/Deep/A",
                "Colors",
                "Colors/Common"
            ]
        );
    }

    #[test]
    fn default_paths() {
        let root = Path::new("r");
        let script = |t| Item {
            kind: Kind::Script,
            script_type: Some(t),
            hash: None,
            meta: None,
        };
        assert_eq!(
            default_path(root, "A/B", &script(ScriptType::Module), false),
            Path::new("r/A/B.luau")
        );
        assert_eq!(
            default_path(root, "A/B", &script(ScriptType::Local), true),
            Path::new("r/A/B/init.client.luau")
        );
        assert_eq!(
            default_path(root, "", &script(ScriptType::Module), false),
            Path::new("r/init.luau")
        );
        assert_eq!(
            default_path(root, "A", &Item::folder(), false),
            Path::new("r/A")
        );
        let model = Item {
            kind: Kind::Model,
            script_type: None,
            hash: None,
            meta: None,
        };
        assert_eq!(
            default_path(root, "A/B", &model, false),
            Path::new("r/A/B.rbxm")
        );
    }
}

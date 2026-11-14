//! `twoway build`: applies the current files to a copy of a place, offline.
//!
//! `rojo build` only makes a place from its project tree. This starts from a saved place instead,
//! so the map and everything else in it stay, and lays the mapped files over it. With `--rojo`, the
//! project's own trees are laid over it first.
//!
//! The rule is the same as for live sync: scripts and folders follow the files, listed models are
//! replaced, and anything else in the place is left exactly as it is.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use rbx_dom_weak::types::{Attributes, Enum, Ref, Tags, Variant};
use rbx_dom_weak::{ustr, InstanceBuilder, WeakDom};
use serde_json::Value;

use crate::config::{Config, Mapping};
use crate::modelfile;
use crate::tree::{
    depth, group_key, is_group_key, join_key, parent_key, scan, split_meta, Item, Kind, ModelRule,
    Role, ScanOptions, ScriptType,
};

pub struct BuildOptions {
    pub config: Option<PathBuf>,
    pub base: PathBuf,
    pub output: PathBuf,
    pub rojo: Option<PathBuf>,
    pub place_id: Option<u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildReport {
    pub scripts: usize,
    pub models: usize,
    pub removed: usize,
    pub rojo_trees: usize,
    pub repaired_references: usize,
    pub lost_references: Vec<String>,
    pub kept: Vec<String>,
}

pub(crate) fn read_place(path: &Path) -> Result<WeakDom> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    rbx_binary::from_reader(BufReader::new(file))
        .with_context(|| format!("could not read {}", path.display()))
}

fn write_place(path: &Path, dom: &WeakDom) -> Result<()> {
    let file = File::create(path).with_context(|| format!("could not write {}", path.display()))?;
    rbx_binary::to_writer(BufWriter::new(file), dom, dom.root().children())?;
    Ok(())
}

fn child_named(dom: &WeakDom, parent: Ref, name: &str) -> Option<Ref> {
    let instance = dom.get_by_ref(parent)?;
    instance
        .children()
        .iter()
        .copied()
        .find(|c| dom.get_by_ref(*c).is_some_and(|i| i.name == name))
}

fn find_path(dom: &WeakDom, path: &[&str]) -> Option<Ref> {
    let mut current = dom.root_ref();
    for segment in path {
        current = child_named(dom, current, segment)?;
    }
    Some(current)
}

/// Finds an instance path, making what's missing. A service at the top is made with its own class.
fn ensure_path(dom: &mut WeakDom, path: &[&str]) -> Ref {
    let mut current = dom.root_ref();
    for (index, segment) in path.iter().enumerate() {
        current = match child_named(dom, current, segment) {
            Some(child) => child,
            None => {
                let class = if index == 0 { *segment } else { "Folder" };
                dom.insert(current, InstanceBuilder::new(class).with_name(*segment))
            }
        };
    }
    current
}

pub(crate) fn script_type_of(dom: &WeakDom, referent: Ref) -> Option<ScriptType> {
    let instance = dom.get_by_ref(referent)?;
    match instance.class.as_str() {
        "ModuleScript" => Some(ScriptType::Module),
        "LocalScript" => Some(ScriptType::Client),
        "Script" => match instance.properties.get(&ustr("RunContext")) {
            Some(Variant::Enum(value)) if value.to_u32() == 2 => Some(ScriptType::Client),
            Some(Variant::Enum(value)) if value.to_u32() == 3 => Some(ScriptType::Plugin),
            _ => Some(ScriptType::Server),
        },
        _ => None,
    }
}

/// A script or a Folder: synced itself, with what's inside looked at too.
pub(crate) fn is_container(dom: &WeakDom, referent: Ref) -> bool {
    dom.get_by_ref(referent)
        .is_some_and(|i| i.class == "Folder")
        || script_type_of(dom, referent).is_some()
}

/// What a walk of a place finds under a mapped root.
#[derive(Default)]
pub(crate) struct Found {
    /// Scripts, folders and models, in the same shape as a scan of the files.
    pub items: BTreeMap<String, Ref>,
    /// The loose instances of each folder that groups them, by the group's key.
    pub groups: BTreeMap<String, Vec<Ref>>,
}

/// Anything that isn't ours stops the walk.
pub(crate) fn walk_place(
    dom: &WeakDom,
    referent: Ref,
    key: &str,
    models: &ModelRule,
    found: &mut Found,
) {
    found.items.insert(key.to_string(), referent);
    let Some(instance) = dom.get_by_ref(referent) else {
        return;
    };
    for child in instance.children() {
        let Some(child_instance) = dom.get_by_ref(*child) else {
            continue;
        };
        let child_key = join_key(key, &child_instance.name);
        match models.role(&child_key, is_container(dom, *child)) {
            Role::Model => {
                found.items.insert(child_key, *child);
            }
            Role::Grouped => found.groups.entry(group_key(key)).or_default().push(*child),
            Role::Container => walk_place(dom, *child, &child_key, models, found),
            Role::Skipped => {}
        }
    }
}

fn is_inside(dom: &WeakDom, mut referent: Ref, ancestor: Ref) -> bool {
    loop {
        if referent == ancestor {
            return true;
        }
        match dom.get_by_ref(referent) {
            Some(instance) if instance.parent().is_some() => referent = instance.parent(),
            _ => return false,
        }
    }
}

/// The names from `ancestor` down to `referent`.
fn path_from(dom: &WeakDom, ancestor: Ref, mut referent: Ref) -> Vec<String> {
    let mut names = Vec::new();
    while referent != ancestor {
        let Some(instance) = dom.get_by_ref(referent) else {
            break;
        };
        names.push(instance.name.clone());
        referent = instance.parent();
    }
    names.reverse();
    names
}

struct OutsideRef {
    holder: Ref,
    property: rbx_dom_weak::Ustr,
    path: Vec<String>,
}

/// Puts `new` where `old` is. References from outside into the old tree follow the instance at the
/// same path in the new one. If there's none, the reference is cleared and reported.
fn replace_subtree(
    dom: &mut WeakDom,
    old: Ref,
    source: &mut WeakDom,
    new: Ref,
    report: &mut BuildReport,
) -> Result<Ref> {
    let parent = dom
        .get_by_ref(old)
        .ok_or_else(|| anyhow!("instance vanished"))?
        .parent();

    let mut outside = Vec::new();
    let ids: Vec<Ref> = dom.descendants().map(|i| i.referent()).collect();
    for holder in ids {
        if is_inside(dom, holder, old) {
            continue;
        }
        let Some(instance) = dom.get_by_ref(holder) else {
            continue;
        };
        for (property, value) in &instance.properties {
            if let Variant::Ref(target) = value {
                if target.is_some() && is_inside(dom, *target, old) {
                    outside.push(OutsideRef {
                        holder,
                        property: *property,
                        path: path_from(dom, old, *target),
                    });
                }
            }
        }
    }

    dom.destroy(old);
    source.transfer(new, dom, parent);

    for reference in outside {
        let mut target = Some(new);
        for name in &reference.path {
            target = target.and_then(|t| child_named(dom, t, name));
        }
        let holder_name = dom
            .get_by_ref(reference.holder)
            .map_or(String::new(), |i| i.name.clone());
        let Some(holder) = dom.get_by_ref_mut(reference.holder) else {
            continue;
        };
        match target {
            Some(referent) => {
                holder
                    .properties
                    .insert(reference.property, Variant::Ref(referent));
                report.repaired_references += 1;
            }
            None => {
                holder
                    .properties
                    .insert(reference.property, Variant::Ref(Ref::none()));
                report
                    .lost_references
                    .push(format!("{holder_name}.{}", reference.property));
            }
        }
    }
    Ok(new)
}

fn set_script(dom: &mut WeakDom, referent: Ref, item: &Item, source: &str) -> Result<()> {
    let wanted = item.script_type.unwrap_or(ScriptType::Module);
    let same_family =
        script_type_of(dom, referent).map(ScriptType::family) == Some(wanted.family());
    let instance = dom
        .get_by_ref_mut(referent)
        .ok_or_else(|| anyhow!("instance vanished"))?;
    if !same_family {
        instance.class = ustr(wanted.class_name());
        match wanted.run_context() {
            Some(context) => {
                instance
                    .properties
                    .insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(context)));
            }
            None => {
                instance.properties.remove(&ustr("RunContext"));
            }
        }
    }
    instance
        .properties
        .insert(ustr("Source"), Variant::String(source.to_string()));
    Ok(())
}

/// Sets the attributes and tags from an item's meta file. Without one, the place's own stay: a
/// folder made by Script Sync never had a way to record them.
fn apply_meta(dom: &mut WeakDom, referent: Ref, item: &Item) -> Result<()> {
    let Some(meta) = item.meta.as_deref() else {
        return Ok(());
    };
    let (attributes, tags) = split_meta(Some(meta));
    let mut set = Attributes::new();
    for (name, value) in attributes {
        let variant = match value {
            Value::String(text) => Variant::String(text),
            Value::Bool(flag) => Variant::Bool(flag),
            Value::Number(number) => Variant::Float64(number.as_f64().unwrap_or_default()),
            other => serde_json::from_value::<Variant>(other.clone()).with_context(|| {
                format!("attribute {name} has a value that isn't understood: {other}")
            })?,
        };
        set.insert(name, variant);
    }
    let instance = dom
        .get_by_ref_mut(referent)
        .ok_or_else(|| anyhow!("instance vanished"))?;
    instance
        .properties
        .insert(ustr("Attributes"), Variant::Attributes(set));
    instance
        .properties
        .insert(ustr("Tags"), Variant::Tags(Tags::from(tags)));
    Ok(())
}

fn read_model_roots(path: &Path) -> Result<(WeakDom, Vec<Ref>)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not open {}", path.display()))?;
    let dom =
        modelfile::decode(&bytes).with_context(|| format!("could not read {}", path.display()))?;
    let roots = dom.root().children().to_vec();
    Ok((dom, roots))
}

/// Puts a folder's loose instances in place: each one replaces the old one of the same name, in
/// order, so references into it are kept. Old ones left over go; new ones left over are added.
fn replace_group(
    dom: &mut WeakDom,
    container: Ref,
    mut old: Vec<Ref>,
    source: &mut WeakDom,
    new: Vec<Ref>,
    report: &mut BuildReport,
) -> Result<()> {
    for referent in new {
        let name = source
            .get_by_ref(referent)
            .map_or(String::new(), |i| i.name.clone());
        let matching = old
            .iter()
            .position(|r| dom.get_by_ref(*r).is_some_and(|i| i.name == name));
        match matching {
            Some(index) => {
                let previous = old.remove(index);
                replace_subtree(dom, previous, source, referent, report)?;
            }
            None => source.transfer(referent, dom, container),
        }
    }
    for gone in old {
        dom.destroy(gone);
        report.removed += 1;
    }
    Ok(())
}

fn read_model(path: &Path) -> Result<(WeakDom, Ref)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not open {}", path.display()))?;
    let dom =
        modelfile::decode(&bytes).with_context(|| format!("could not read {}", path.display()))?;
    match dom.root().children() {
        [only] => {
            let only = *only;
            Ok((dom, only))
        }
        children => bail!(
            "{} holds {} roots, expected 1",
            path.display(),
            children.len()
        ),
    }
}

fn apply_mapping(
    dom: &mut WeakDom,
    root: &Path,
    mapping: &Mapping,
    config: &Config,
    report: &mut BuildReport,
) -> Result<()> {
    let models = ModelRule::for_mapping(mapping)?;
    let ignore = config.ignore_set()?;
    let disk = scan(
        &root.join(&mapping.path),
        &ScanOptions {
            models: &models,
            ignore: &ignore,
        },
    )?;
    if disk.items.is_empty() {
        eprintln!(
            "warning: {} has no files, leaving {} alone",
            mapping.path, mapping.studio
        );
        return Ok(());
    }

    let path: Vec<&str> = mapping.studio.split('/').collect();
    let mapped_root = ensure_path(dom, &path);
    let mut found = Found::default();
    walk_place(dom, mapped_root, "", &models, &mut found);
    let Found {
        items: mut existing,
        mut groups,
    } = found;

    let mut keys: Vec<&String> = disk.items.keys().collect();
    keys.sort_by_key(|key| depth(key));

    for key in keys {
        let item = &disk.items[key];
        let name = key.rsplit('/').next().unwrap_or(key);
        let parent = parent_key(key).and_then(|p| existing.get(p).copied());
        let current = existing.get(key.as_str()).copied();
        match item.kind {
            Kind::Folder => {
                let referent = match current {
                    Some(referent) => referent,
                    None => {
                        let parent = parent.ok_or_else(|| anyhow!("no parent for {key}"))?;
                        let made =
                            dom.insert(parent, InstanceBuilder::new("Folder").with_name(name));
                        existing.insert(key.clone(), made);
                        made
                    }
                };
                apply_meta(dom, referent, item)?;
            }
            Kind::Script => {
                let referent = match current {
                    Some(referent) => referent,
                    None => {
                        let parent = parent.ok_or_else(|| anyhow!("no parent for {key}"))?;
                        let class = item.script_type.unwrap_or(ScriptType::Module).class_name();
                        let made = dom.insert(parent, InstanceBuilder::new(class).with_name(name));
                        existing.insert(key.clone(), made);
                        made
                    }
                };
                // Force the class on an instance we just made or on one that differs.
                set_script(dom, referent, item, &disk.sources[key])?;
                apply_meta(dom, referent, item)?;
                report.scripts += 1;
            }
            Kind::Model if is_group_key(key) => {
                let (mut source, new) = read_model_roots(&disk.paths[key])?;
                let container = parent.ok_or_else(|| anyhow!("no folder for {key}"))?;
                let old = groups.remove(key.as_str()).unwrap_or_default();
                replace_group(dom, container, old, &mut source, new, report)?;
                report.models += 1;
            }
            Kind::Model => {
                let (mut source, new) = read_model(&disk.paths[key])?;
                source
                    .get_by_ref_mut(new)
                    .ok_or_else(|| anyhow!("model root vanished"))?
                    .name = name.to_string();
                let referent = match current {
                    Some(old) => replace_subtree(dom, old, &mut source, new, report)?,
                    None => {
                        let parent = parent.ok_or_else(|| anyhow!("no parent for {key}"))?;
                        source.transfer(new, dom, parent);
                        new
                    }
                };
                existing.insert(key.clone(), referent);
                report.models += 1;
            }
        }
    }

    // What the files no longer have goes too, deepest first. Whatever still holds something we
    // don't manage stays, as in live sync.
    // A folder with nothing synced inside it isn't ours, as in live sync.
    let is_folder =
        |dom: &WeakDom, r: &Ref| dom.get_by_ref(*r).is_some_and(|i| i.class == "Folder");
    let synced_below = |key: &str| {
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!("{key}/")
        };
        existing
            .iter()
            .any(|(k, r)| k.starts_with(&prefix) && k.as_str() != key && !is_folder(dom, r))
            || groups.keys().any(|k| k.starts_with(&prefix))
    };
    let mut gone: Vec<(&String, &Ref)> = existing
        .iter()
        .filter(|(k, r)| !disk.items.contains_key(*k) && (!is_folder(dom, r) || synced_below(k)))
        .collect();
    gone.sort_by_key(|(key, _)| std::cmp::Reverse(depth(key)));
    // A folder whose shared file is gone loses its loose instances, as a model would.
    for (key, members) in &groups {
        if disk.items.contains_key(key) {
            continue;
        }
        for member in members {
            if dom
                .get_by_ref(*member)
                .is_some_and(|i| i.children().is_empty())
            {
                dom.destroy(*member);
                report.removed += 1;
            } else {
                report.kept.push(format!("{}/{key}", mapping.studio));
            }
        }
    }
    for (key, referent) in gone {
        let empty = dom
            .get_by_ref(*referent)
            .is_some_and(|i| i.children().is_empty());
        if empty {
            dom.destroy(*referent);
            report.removed += 1;
        } else {
            report.kept.push(format!("{}/{key}", mapping.studio));
        }
    }
    Ok(())
}

/// The instance paths in a Rojo project that come from files, and the nodes that set properties.
fn project_nodes(
    node: &Value,
    path: Vec<String>,
    trees: &mut Vec<Vec<String>>,
    properties: &mut Vec<Vec<String>>,
) {
    let Some(object) = node.as_object() else {
        return;
    };
    if object.contains_key("$path") {
        trees.push(path);
        return;
    }
    if object.contains_key("$properties") {
        properties.push(path.clone());
    }
    for (name, child) in object {
        if !name.starts_with('$') {
            let mut child_path = path.clone();
            child_path.push(name.clone());
            project_nodes(child, child_path, trees, properties);
        }
    }
}

fn apply_rojo(dom: &mut WeakDom, project: &Path, report: &mut BuildReport) -> Result<()> {
    let text = std::fs::read_to_string(project)
        .with_context(|| format!("could not read {}", project.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", project.display()))?;

    let temp_dir = std::env::temp_dir().join(format!("twoway-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)?;
    let built_path = temp_dir.join("rojo.rbxl");
    let status = Command::new("rojo")
        .arg("build")
        .arg(project)
        .arg("-o")
        .arg(&built_path)
        .status()
        .context("could not run rojo; is it on the PATH?")?;
    if !status.success() {
        bail!("rojo build failed");
    }
    let mut built = read_place(&built_path)?;
    let _ = std::fs::remove_dir_all(&temp_dir);

    let (mut trees, mut properties) = (Vec::new(), Vec::new());
    project_nodes(&value["tree"], Vec::new(), &mut trees, &mut properties);

    for path in properties {
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let Some(from) = find_path(&built, &names) else {
            continue;
        };
        let target = ensure_path(dom, &names);
        let copied = built
            .get_by_ref(from)
            .map(|i| i.properties.clone())
            .unwrap_or_default();
        if let Some(instance) = dom.get_by_ref_mut(target) {
            instance.properties.extend(copied);
        }
    }

    for path in trees {
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let Some(new) = find_path(&built, &names) else {
            eprintln!("warning: rojo built nothing at {}", path.join("/"));
            continue;
        };
        match find_path(dom, &names) {
            Some(old) => {
                replace_subtree(dom, old, &mut built, new, report)?;
            }
            None => {
                let parent = ensure_path(dom, &names[..names.len() - 1]);
                built.transfer(new, dom, parent);
            }
        }
        report.rojo_trees += 1;
    }
    Ok(())
}

/// Which mappings go into this place: those that name no place, and those that name this one.
pub(crate) fn mappings_for_build<'a>(
    config: &'a Config,
    place_id: Option<u64>,
    base: &Path,
) -> Vec<&'a Mapping> {
    let file_name = base.file_name().and_then(|n| n.to_str()).unwrap_or("");
    config
        .mappings
        .iter()
        .filter(|m| {
            !m.names_any_place()
                || place_id.is_some_and(|id| m.place_id == Some(id))
                || m.place_name.as_deref() == Some(file_name)
        })
        .collect()
}

pub fn build_place(options: &BuildOptions) -> Result<BuildReport> {
    let mut report = BuildReport::default();
    let mut dom = read_place(&options.base)?;

    if let Some(project) = &options.rojo {
        apply_rojo(&mut dom, project, &mut report)?;
    }
    if let Some(config_path) = &options.config {
        let (config, root) = Config::load(config_path)?;
        let mappings = mappings_for_build(&config, options.place_id, &options.base);
        for mapping in mappings {
            apply_mapping(&mut dom, &root, mapping, &config, &mut report)?;
        }
    }

    // A temporary name, so a failed write never leaves a half-written place.
    let temp = options.output.with_extension("rbxl.tmp");
    write_place(&temp, &dom)?;
    std::fs::rename(&temp, &options.output)?;
    Ok(report)
}

/// Used by tests and by nothing that writes: every instance's path and class.
#[cfg(test)]
fn census(dom: &WeakDom) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for instance in dom.descendants() {
        let path = path_from(dom, dom.root_ref(), instance.referent()).join("/");
        map.insert(path, instance.class.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn place() -> WeakDom {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let rs = dom.insert(
            root,
            InstanceBuilder::new("ReplicatedStorage").with_name("ReplicatedStorage"),
        );
        let ws = dom.insert(
            root,
            InstanceBuilder::new("Workspace").with_name("Workspace"),
        );
        dom.insert(ws, InstanceBuilder::new("Part").with_name("Map"));

        let kit = dom.insert(rs, InstanceBuilder::new("Folder").with_name("Kit"));
        dom.insert(
            kit,
            InstanceBuilder::new("ModuleScript")
                .with_name("Old")
                .with_property("Source", "old"),
        );
        dom.insert(
            kit,
            InstanceBuilder::new("ModuleScript")
                .with_name("Gone")
                .with_property("Source", "gone"),
        );
        dom.insert(
            kit,
            InstanceBuilder::new("Configuration").with_name("StudioOnly"),
        );

        let assets = dom.insert(kit, InstanceBuilder::new("Folder").with_name("Assets"));
        let inner = dom.insert(assets, InstanceBuilder::new("Part").with_name("Inner"));
        let dropped = dom.insert(assets, InstanceBuilder::new("Part").with_name("Dropped"));
        dom.insert(
            ws,
            InstanceBuilder::new("ObjectValue")
                .with_name("Link")
                .with_property("Value", inner),
        );
        dom.insert(
            ws,
            InstanceBuilder::new("ObjectValue")
                .with_name("Broken")
                .with_property("Value", dropped),
        );
        dom
    }

    fn model_file(path: &Path) {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let assets = dom.insert(root, InstanceBuilder::new("Folder").with_name("Assets"));
        dom.insert(assets, InstanceBuilder::new("Part").with_name("Inner"));
        dom.insert(assets, InstanceBuilder::new("Sound").with_name("New"));
        write_place(path, &dom).unwrap();
    }

    fn setup() -> (tempfile::TempDir, BuildOptions) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("files/Sub")).unwrap();
        fs::write(root.join("files/Old.luau"), "new text").unwrap();
        fs::write(root.join("files/Sub/Added.client.luau"), "added").unwrap();
        model_file(&root.join("files/Assets.rbxm"));
        fs::write(
            root.join("twoway.json"),
            r#"{ "mappings": [
                { "studio": "ReplicatedStorage/Kit", "path": "files", "models": ["Assets"], "instances": "listed" },
                { "studio": "ReplicatedStorage/Elsewhere", "path": "other", "placeId": 99 }
            ] }"#,
        )
        .unwrap();
        write_place(&root.join("base.rbxl"), &place()).unwrap();
        let options = BuildOptions {
            config: Some(root.join("twoway.json")),
            base: root.join("base.rbxl"),
            output: root.join("out.rbxl"),
            rojo: None,
            place_id: None,
        };
        (dir, options)
    }

    #[test]
    fn files_land_in_the_place_and_the_rest_stays() {
        let (_dir, options) = setup();
        let report = build_place(&options).unwrap();
        let dom = read_place(&options.output).unwrap();
        let classes = census(&dom);

        let old = find_path(&dom, &["ReplicatedStorage", "Kit", "Old"]).unwrap();
        assert_eq!(
            dom.get_by_ref(old).unwrap().properties.get(&ustr("Source")),
            Some(&Variant::String("new text".into()))
        );
        assert_eq!(classes["ReplicatedStorage/Kit/Sub/Added"], "LocalScript");
        assert_eq!(classes["ReplicatedStorage/Kit/Assets/New"], "Sound");
        assert!(
            !classes.contains_key("ReplicatedStorage/Kit/Assets/Dropped"),
            "the model replaces the tree"
        );
        assert!(
            !classes.contains_key("ReplicatedStorage/Kit/Gone"),
            "its file is gone"
        );
        assert_eq!(
            classes["ReplicatedStorage/Kit/StudioOnly"], "Configuration",
            "not ours"
        );
        assert_eq!(classes["Workspace/Map"], "Part");
        assert_eq!(report.scripts, 2);
        assert_eq!(report.models, 1);
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn a_folders_shared_file_replaces_its_loose_instances() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("files")).unwrap();
        fs::write(root.join("files/Old.luau"), "new text").unwrap();
        let mut file = WeakDom::new(InstanceBuilder::new("DataModel"));
        let top = file.root_ref();
        let config = file.insert(
            top,
            InstanceBuilder::new("Configuration").with_name("StudioOnly"),
        );
        let extra = file.insert(top, InstanceBuilder::new("Sound").with_name("Extra"));
        fs::write(
            root.join("files/init.rbxmx"),
            modelfile::encode(&file, &[config, extra], modelfile::ModelFormat::Rbxmx).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("twoway.json"),
            r#"{ "mappings": [ { "studio": "ReplicatedStorage/Kit", "path": "files", "instances": "auto" } ] }"#,
        )
        .unwrap();
        write_place(&root.join("base.rbxl"), &place()).unwrap();
        let options = BuildOptions {
            config: Some(root.join("twoway.json")),
            base: root.join("base.rbxl"),
            output: root.join("out.rbxl"),
            rojo: None,
            place_id: None,
        };
        build_place(&options).unwrap();
        let classes = census(&read_place(&options.output).unwrap());
        assert_eq!(classes["ReplicatedStorage/Kit/StudioOnly"], "Configuration");
        assert_eq!(
            classes["ReplicatedStorage/Kit/Extra"], "Sound",
            "added from the file"
        );
        assert!(
            !classes.contains_key("ReplicatedStorage/Kit/Assets/Inner"),
            "Assets has no file for its instances"
        );
        assert!(!classes.contains_key("ReplicatedStorage/Kit/Gone"));
    }

    #[test]
    fn references_into_a_replaced_model_follow_the_path() {
        let (_dir, options) = setup();
        let report = build_place(&options).unwrap();
        let dom = read_place(&options.output).unwrap();

        let link = find_path(&dom, &["Workspace", "Link"]).unwrap();
        let inner = find_path(&dom, &["ReplicatedStorage", "Kit", "Assets", "Inner"]).unwrap();
        assert_eq!(
            dom.get_by_ref(link).unwrap().properties.get(&ustr("Value")),
            Some(&Variant::Ref(inner))
        );
        assert_eq!(report.repaired_references, 1);
        assert_eq!(report.lost_references, ["Broken.Value"]);
    }

    #[test]
    fn meta_files_set_attributes_and_tags() {
        let (dir, options) = setup();
        fs::write(
            dir.path().join("files/Old.meta.json"),
            r#"{ "attributes": { "Offset": { "Vector3": [0, -1, 0] }, "Label": "hi", "Count": 3 }, "properties": { "Tags": ["Spawn"] } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("files/Sub/init.meta.json"),
            r#"{ "attributes": { "On": true } }"#,
        )
        .unwrap();
        build_place(&options).unwrap();
        let dom = read_place(&options.output).unwrap();

        let old = dom
            .get_by_ref(find_path(&dom, &["ReplicatedStorage", "Kit", "Old"]).unwrap())
            .unwrap();
        let Some(Variant::Attributes(attributes)) = old.properties.get(&ustr("Attributes")) else {
            panic!("no attributes")
        };
        assert_eq!(
            attributes.get("Offset"),
            Some(&Variant::Vector3(rbx_dom_weak::types::Vector3::new(
                0.0, -1.0, 0.0
            )))
        );
        match attributes.get("Label") {
            Some(Variant::String(text)) => assert_eq!(text, "hi"),
            Some(Variant::BinaryString(bytes)) => assert_eq!(AsRef::<[u8]>::as_ref(bytes), b"hi"),
            other => panic!("Label is {other:?}"),
        }
        assert_eq!(attributes.get("Count"), Some(&Variant::Float64(3.0)));
        let Some(Variant::Tags(tags)) = old.properties.get(&ustr("Tags")) else {
            panic!("no tags")
        };
        assert_eq!(tags.iter().collect::<Vec<_>>(), ["Spawn"]);

        let sub = dom
            .get_by_ref(find_path(&dom, &["ReplicatedStorage", "Kit", "Sub"]).unwrap())
            .unwrap();
        let Some(Variant::Attributes(attributes)) = sub.properties.get(&ustr("Attributes")) else {
            panic!("no attributes")
        };
        assert_eq!(attributes.get("On"), Some(&Variant::Bool(true)));
    }

    #[test]
    fn without_a_meta_file_the_place_keeps_its_attributes() {
        let (_dir, options) = setup();
        let mut dom = place();
        let old = find_path(&dom, &["ReplicatedStorage", "Kit", "Old"]).unwrap();
        let mut attributes = Attributes::new();
        attributes.insert("Kept".into(), Variant::Bool(true));
        dom.get_by_ref_mut(old)
            .unwrap()
            .properties
            .insert(ustr("Attributes"), Variant::Attributes(attributes));
        write_place(&options.base, &dom).unwrap();

        build_place(&options).unwrap();
        let built = read_place(&options.output).unwrap();
        let old = built
            .get_by_ref(find_path(&built, &["ReplicatedStorage", "Kit", "Old"]).unwrap())
            .unwrap();
        let Some(Variant::Attributes(attributes)) = old.properties.get(&ustr("Attributes")) else {
            panic!("lost")
        };
        assert_eq!(attributes.get("Kept"), Some(&Variant::Bool(true)));
    }

    #[test]
    fn the_base_is_only_read() {
        let (_dir, options) = setup();
        let before = fs::read(&options.base).unwrap();
        build_place(&options).unwrap();
        assert_eq!(fs::read(&options.base).unwrap(), before);
    }

    #[test]
    fn mappings_for_another_place_are_skipped() {
        let (dir, mut options) = setup();
        fs::create_dir_all(dir.path().join("other")).unwrap();
        fs::write(dir.path().join("other/X.luau"), "x").unwrap();

        build_place(&options).unwrap();
        assert!(!census(&read_place(&options.output).unwrap())
            .contains_key("ReplicatedStorage/Elsewhere/X"));

        options.place_id = Some(99);
        build_place(&options).unwrap();
        assert!(census(&read_place(&options.output).unwrap())
            .contains_key("ReplicatedStorage/Elsewhere/X"));
    }

    #[test]
    fn a_folder_with_unmanaged_children_is_kept_when_its_directory_is_gone() {
        let (dir, options) = setup();
        let mut dom = place();
        let kit = find_path(&dom, &["ReplicatedStorage", "Kit"]).unwrap();
        let folder = dom.insert(kit, InstanceBuilder::new("Folder").with_name("Actors"));
        dom.insert(folder, InstanceBuilder::new("Model").with_name("Girl"));
        write_place(&options.base, &dom).unwrap();

        let report = build_place(&options).unwrap();
        assert!(census(&read_place(&options.output).unwrap())
            .contains_key("ReplicatedStorage/Kit/Actors/Girl"));
        assert!(
            report.kept.is_empty(),
            "a folder with nothing synced in it is not ours to report"
        );
        drop(dir);
    }

    #[test]
    fn project_nodes_finds_file_trees_and_properties() {
        let tree: Value = serde_json::from_str(
            r#"{ "$className": "DataModel",
                 "ServerScriptService": { "TS": { "$path": "out/server" } },
                 "ReplicatedStorage": { "rbxts_include": { "$path": "include", "node_modules": { "$className": "Folder" } } },
                 "HttpService": { "$properties": { "HttpEnabled": true } } }"#,
        )
        .unwrap();
        let (mut trees, mut properties) = (Vec::new(), Vec::new());
        project_nodes(&tree, Vec::new(), &mut trees, &mut properties);
        assert_eq!(
            trees,
            [
                vec!["ReplicatedStorage", "rbxts_include"],
                vec!["ServerScriptService", "TS"]
            ]
        );
        assert_eq!(properties, [vec!["HttpService"]]);
    }
}

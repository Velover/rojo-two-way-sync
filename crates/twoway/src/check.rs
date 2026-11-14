//! `rojo twoway check`: compares the mapped folders with a saved place, without Studio, and says what a
//! first sync would do. Nothing is written.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{ustr, WeakDom};
use serde_json::{Map, Value};

use crate::config::{Config, Mapping};
use crate::modeldiff;
use crate::modelfile;
use crate::place_build::{
    is_container, mappings_for_place, place_file_name, read_place, script_type_of, walk_place,
    Found,
};
use crate::tree::{
    canonical_meta, hash_bytes, is_group_key, normalize_text, parent_key, prune_folders, scan,
    DiskTree, Item, Kind, ModelRule, Role, ScanOptions, ScriptType,
};

/// An attribute value as a Rojo meta file writes it.
fn attribute_json(value: &Variant) -> Option<Value> {
    Some(match value {
        Variant::String(text) => Value::String(text.clone()),
        Variant::BinaryString(bytes) => {
            Value::String(String::from_utf8(AsRef::<[u8]>::as_ref(bytes).to_vec()).ok()?)
        }
        Variant::Bool(flag) => Value::Bool(*flag),
        Variant::Float64(number) => Value::from(*number),
        Variant::Float32(number) => Value::from(*number as f64),
        Variant::Int32(number) => Value::from(*number),
        Variant::Int64(number) => Value::from(*number),
        other => serde_json::to_value(other).ok()?,
    })
}

fn instance_meta(dom: &WeakDom, referent: Ref) -> Option<String> {
    let instance = dom.get_by_ref(referent)?;
    let mut attributes = Map::new();
    if let Some(Variant::Attributes(list)) = instance.properties.get(&ustr("Attributes")) {
        for (name, value) in list.iter() {
            if let Some(json) = attribute_json(value) {
                attributes.insert(name.clone(), json);
            }
        }
    }
    let tags: Vec<String> = match instance.properties.get(&ustr("Tags")) {
        Some(Variant::Tags(tags)) => tags.iter().map(str::to_string).collect(),
        _ => Vec::new(),
    };
    canonical_meta(Some(&Value::Object(attributes)), Some(&tags))
}

struct PlaceSide {
    items: BTreeMap<String, Item>,
    sources: BTreeMap<String, String>,
    models: BTreeMap<String, Vec<u8>>,
    found: bool,
}

fn read_place_side(dom: &WeakDom, mapping: &Mapping) -> Result<PlaceSide> {
    let models = ModelRule::for_mapping(mapping)?;
    let mut side = PlaceSide {
        items: BTreeMap::new(),
        sources: BTreeMap::new(),
        models: BTreeMap::new(),
        found: false,
    };

    let mut current = Some(dom.root_ref());
    for segment in mapping.studio.split('/') {
        current = current.and_then(|parent| {
            dom.get_by_ref(parent)?
                .children()
                .iter()
                .copied()
                .find(|c| dom.get_by_ref(*c).is_some_and(|i| i.name == segment))
        });
    }
    let Some(root) = current else { return Ok(side) };
    side.found = true;

    let mut found = Found::default();
    walk_place(dom, root, "", &models, &mut found);
    for (key, members) in found.groups {
        let bytes = modelfile::encode(dom, &members, models.format)?;
        side.items.insert(
            key.clone(),
            Item {
                kind: Kind::Model,
                script_type: None,
                hash: Some(hash_bytes(&bytes)),
                meta: None,
            },
        );
        side.models.insert(key, bytes);
    }
    for (key, referent) in found.items {
        let meta = instance_meta(dom, referent);
        let item =
            if !key.is_empty() && models.role(&key, is_container(dom, referent)) == Role::Model {
                let bytes = modelfile::encode(dom, &[referent], models.format)?;
                let item = Item {
                    kind: Kind::Model,
                    script_type: None,
                    hash: Some(hash_bytes(&bytes)),
                    meta: None,
                };
                side.models.insert(key.clone(), bytes);
                item
            } else if let Some(script_type) = script_type_of(dom, referent) {
                let source = match dom
                    .get_by_ref(referent)
                    .and_then(|i| i.properties.get(&ustr("Source")))
                {
                    Some(Variant::String(text)) => normalize_text(text),
                    _ => String::new(),
                };
                let item = Item {
                    kind: Kind::Script,
                    script_type: Some(script_type),
                    hash: Some(hash_bytes(source.as_bytes())),
                    meta,
                };
                side.sources.insert(key.clone(), source);
                item
            } else {
                Item {
                    meta,
                    ..Item::folder()
                }
            };
        side.items.insert(key, item);
    }
    prune_folders(&mut side.items);
    Ok(side)
}

fn describe(item: &Item) -> &'static str {
    match (item.kind, item.script_type.map(ScriptType::family)) {
        (Kind::Folder, _) if item.meta.is_some() => "Folder with attributes or tags",
        (Kind::Folder, _) => "Folder",
        (Kind::Model, _) => "model",
        (_, Some(ScriptType::Client)) => "client script",
        (_, Some(ScriptType::Server)) => "server script",
        (_, Some(ScriptType::Plugin)) => "plugin script",
        _ => "ModuleScript",
    }
}

fn changed_lines(a: &str, b: &str) -> (usize, usize) {
    let patch = diffy::create_patch(a, b).to_string();
    let added = patch
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let removed = patch
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    (added, removed)
}

#[derive(Default)]
struct Report {
    same: usize,
    to_studio: Vec<String>,
    to_disk: Vec<String>,
    review: Vec<String>,
    notes: Vec<String>,
}

fn compare(
    mapping: &Mapping,
    disk: &DiskTree,
    place: &PlaceSide,
    report: &mut Report,
) -> Result<()> {
    let keys: BTreeSet<&String> = disk.items.keys().chain(place.items.keys()).collect();
    for key in keys {
        let label = if key.is_empty() {
            format!("{} (the root)", mapping.studio)
        } else if is_group_key(key) {
            let folder = parent_key(key).unwrap_or("");
            let folder = if folder.is_empty() {
                mapping.studio.clone()
            } else {
                format!("{}/{folder}", mapping.studio)
            };
            format!("{folder}: its instances, in one file")
        } else {
            format!("{}/{key}", mapping.studio)
        };
        match (place.items.get(key), disk.items.get(key)) {
            (Some(p), Some(d)) => {
                let mut p = p.clone();
                if p.kind == Kind::Model && d.kind == Kind::Model {
                    let disk_bytes = std::fs::read(&disk.paths[key])?;
                    if modeldiff::diff(&place.models[key], &disk_bytes)?.is_empty() {
                        p.hash = d.hash.clone();
                    }
                }
                if p == *d {
                    report.same += 1;
                    continue;
                }
                let mut why = Vec::new();
                if !p.same_shape(d) {
                    why.push(format!(
                        "place has a {}, the file is a {}",
                        describe(&p),
                        describe(d)
                    ));
                }
                if p.hash != d.hash {
                    if let (Some(a), Some(b)) = (disk.sources.get(key), place.sources.get(key)) {
                        let (added, removed) = changed_lines(a, b);
                        why.push(format!("text differs: the place has {added} line(s) the file doesn't, the file has {removed} the place doesn't"));
                    } else if p.kind == Kind::Model {
                        why.push("instances differ".into());
                    }
                }
                if p.meta != d.meta {
                    if p.same_shape(d) && p.hash == d.hash && d.meta.is_none() {
                        report.to_disk.push(format!(
                            "{label}: its attributes or tags get written to a meta file"
                        ));
                        continue;
                    }
                    why.push("attributes or tags differ".into());
                }
                report.review.push(format!("{label}: {}", why.join("; ")));
            }
            (None, Some(d)) => report.to_studio.push(format!("{label} ({})", describe(d))),
            (Some(p), None) => report.to_disk.push(format!("{label} ({})", describe(p))),
            (None, None) => {}
        }
    }
    Ok(())
}

pub fn check(config_path: &Path, place_path: &Path, place_id: Option<u64>) -> Result<()> {
    let (config, root) = Config::load(config_path)?;
    let dom = read_place(place_path)?;
    let ignore = config.ignore_set()?;
    let mut report = Report::default();

    let used: Vec<&Mapping> = mappings_for_place(&config, place_id, place_file_name(place_path));
    for mapping in &config.mappings {
        if !used.iter().any(|m| std::ptr::eq(*m, mapping)) {
            println!("skipped {}: it names another place", mapping.studio);
        }
    }
    for mapping in used {
        let models = ModelRule::for_mapping(mapping)?;
        let disk = scan(
            &root.join(&mapping.path),
            &ScanOptions {
                models: &models,
                ignore: &ignore,
            },
        )?;
        report.notes.extend(disk.warnings.iter().cloned());
        let place = read_place_side(&dom, mapping)?;
        if !place.found {
            report.notes.push(format!(
                "{} isn't in the place: a first sync creates it from the files",
                mapping.studio
            ));
        }
        if disk.items.is_empty() {
            report.notes.push(format!(
                "{} has no files: a first sync writes them from Studio",
                mapping.path
            ));
        }
        compare(mapping, &disk, &place, &mut report)?;
    }

    println!("\n{} item(s) are the same on both sides.", report.same);
    let section = |title: &str, list: &[String]| {
        if !list.is_empty() {
            println!("\n{title} ({}):", list.len());
            for line in list {
                println!("  {line}");
            }
        }
    };
    section(
        "Only in the files, so a first sync adds them to Studio",
        &report.to_studio,
    );
    section(
        "Only in the place, so a first sync writes them to disk",
        &report.to_disk,
    );
    section(
        "Different on each side, so a first sync asks you",
        &report.review,
    );
    section("Notes", &report.notes);
    println!(
        "\nThis compares with the saved place. Studio may hold newer edits, so the real first sync can differ."
    );
    Ok(())
}

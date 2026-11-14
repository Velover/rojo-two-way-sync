//! A read-only comparison of two `.rbxm` files, for the review page. Instances are matched by
//! name under their parent, and repeated names pair up in order.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{Instance, WeakDom};
use serde::Serialize;

const MAX_VALUE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Same,
    Changed,
    OnlyStudio,
    OnlyDisk,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyChange {
    pub name: String,
    pub studio: Option<String>,
    pub disk: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub name: String,
    pub class: String,
    pub status: Status,
    pub properties: Vec<PropertyChange>,
    pub children: Vec<Node>,
    /// How many instances are below this one when the list is left out.
    pub descendants: usize,
}

fn read(bytes: &[u8]) -> Result<WeakDom> {
    crate::modelfile::decode(bytes)
}

fn plain(value: &Variant) -> String {
    match value {
        Variant::String(text) => text.clone(),
        Variant::Float32(number) => number.to_string(),
        Variant::Float64(number) => number.to_string(),
        Variant::Int32(number) => number.to_string(),
        Variant::Int64(number) => number.to_string(),
        Variant::Bool(value) => value.to_string(),
        Variant::Attributes(attributes) => {
            let mut parts: Vec<String> = attributes
                .iter()
                .map(|(name, value)| format!("{name} = {}", plain(value)))
                .collect();
            parts.sort();
            parts.join(", ")
        }
        other => format!("{other:?}"),
    }
}

fn show(value: &Variant) -> String {
    let text = plain(value);
    if text.chars().count() > MAX_VALUE_CHARS {
        let short: String = text.chars().take(MAX_VALUE_CHARS).collect();
        format!("{short}...")
    } else {
        text
    }
}

/// References hold random ids that differ between any two files, so they can't be compared.
fn comparable(value: &Variant) -> Option<String> {
    match value {
        Variant::Ref(_) => None,
        other => Some(plain(other)),
    }
}

fn property_changes(studio: &Instance, disk: &Instance) -> Vec<PropertyChange> {
    let names: BTreeSet<&str> = studio
        .properties
        .keys()
        .chain(disk.properties.keys())
        .map(|name| name.as_str())
        .filter(|name| *name != "UniqueId")
        .collect();

    let mut changes = Vec::new();
    for name in names {
        let a = studio
            .properties
            .iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, v)| v);
        let b = disk
            .properties
            .iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, v)| v);
        if a.and_then(comparable) != b.and_then(comparable) {
            changes.push(PropertyChange {
                name: name.to_string(),
                studio: a.map(show),
                disk: b.map(show),
            });
        }
    }
    changes
}

fn count(dom: &WeakDom, referent: Ref) -> usize {
    dom.get_by_ref(referent).map_or(0, |instance| {
        instance
            .children()
            .iter()
            .map(|child| 1 + count(dom, *child))
            .sum()
    })
}

fn only(dom: &WeakDom, referent: Ref, status: Status) -> Option<Node> {
    let instance = dom.get_by_ref(referent)?;
    Some(Node {
        name: instance.name.clone(),
        class: instance.class.to_string(),
        status,
        properties: Vec::new(),
        children: Vec::new(),
        descendants: count(dom, referent),
    })
}

fn compare_children(studio: &WeakDom, a: &[Ref], disk: &WeakDom, b: &[Ref]) -> Vec<Node> {
    let mut unmatched: BTreeMap<&str, Vec<Ref>> = BTreeMap::new();
    for referent in b {
        if let Some(instance) = disk.get_by_ref(*referent) {
            unmatched
                .entry(instance.name.as_str())
                .or_default()
                .push(*referent);
        }
    }

    let mut nodes = Vec::new();
    for referent in a {
        let Some(instance) = studio.get_by_ref(*referent) else {
            continue;
        };
        let partner = unmatched
            .get_mut(instance.name.as_str())
            .and_then(|list| (!list.is_empty()).then(|| list.remove(0)));
        let node = match partner {
            Some(other) => compare(studio, *referent, disk, other),
            None => only(studio, *referent, Status::OnlyStudio),
        };
        nodes.extend(node.filter(|n| n.status != Status::Same));
    }
    for list in unmatched.values() {
        nodes.extend(list.iter().filter_map(|r| only(disk, *r, Status::OnlyDisk)));
    }
    nodes
}

fn compare(studio: &WeakDom, a: Ref, disk: &WeakDom, b: Ref) -> Option<Node> {
    let (x, y) = (studio.get_by_ref(a)?, disk.get_by_ref(b)?);
    let mut properties = property_changes(x, y);
    if x.class != y.class {
        properties.insert(
            0,
            PropertyChange {
                name: "ClassName".into(),
                studio: Some(x.class.to_string()),
                disk: Some(y.class.to_string()),
            },
        );
    }
    let children = compare_children(studio, x.children(), disk, y.children());
    let status = if properties.is_empty() && children.is_empty() {
        Status::Same
    } else {
        Status::Changed
    };
    Some(Node {
        name: x.name.clone(),
        class: x.class.to_string(),
        status,
        properties,
        children,
        descendants: count(studio, a),
    })
}

/// A property one file leaves out compared with the other file's value: the same when that value
/// is the class's default, which is what reading the file gives. When the default isn't known, the
/// property is something Studio adds on its own, and doesn't count.
fn default_like(class: &str, name: &str, value: &Variant) -> bool {
    match value {
        Variant::Attributes(attributes) => return attributes.iter().next().is_none(),
        Variant::Tags(tags) => return tags.iter().next().is_none(),
        Variant::Ref(_) => return true,
        _ => {}
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return true;
    };
    let Some(descriptor) = database.classes.get(class) else {
        return true;
    };
    match database.find_default_property(descriptor, name) {
        Some(default) => comparable(default) == comparable(value),
        None => true,
    }
}

fn same_instance(studio: &WeakDom, a: Ref, disk: &WeakDom, b: Ref) -> bool {
    let (Some(x), Some(y)) = (studio.get_by_ref(a), disk.get_by_ref(b)) else {
        return false;
    };
    if x.class != y.class {
        return false;
    }
    let names: BTreeSet<&str> = x
        .properties
        .keys()
        .chain(y.properties.keys())
        .map(|name| name.as_str())
        .filter(|name| *name != "UniqueId")
        .collect();
    for name in names {
        let find = |instance: &Instance| {
            instance
                .properties
                .iter()
                .find(|(k, _)| k.as_str() == name)
                .map(|(_, v)| v.clone())
        };
        let same = match (find(x), find(y)) {
            (Some(p), Some(q)) => comparable(&p) == comparable(&q),
            (Some(only), None) | (None, Some(only)) => default_like(x.class.as_str(), name, &only),
            (None, None) => true,
        };
        if !same {
            return false;
        }
    }
    same_children(studio, x.children(), disk, y.children())
}

fn same_children(studio: &WeakDom, a: &[Ref], disk: &WeakDom, b: &[Ref]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut unmatched: BTreeMap<&str, Vec<Ref>> = BTreeMap::new();
    for referent in b {
        if let Some(instance) = disk.get_by_ref(*referent) {
            unmatched
                .entry(instance.name.as_str())
                .or_default()
                .push(*referent);
        }
    }
    a.iter().all(|referent| {
        let Some(instance) = studio.get_by_ref(*referent) else {
            return false;
        };
        let partner = unmatched
            .get_mut(instance.name.as_str())
            .and_then(|list| (!list.is_empty()).then(|| list.remove(0)));
        partner.is_some_and(|other| same_instance(studio, *referent, disk, other))
    })
}

/// True when two model files hold the same instances, counting what a file leaves out as the
/// default (a hand-written file names only what it sets). A push uses it so a model it sent
/// doesn't look changed when Studio saves it back with every property.
pub fn same_content(studio: &[u8], disk: &[u8]) -> Result<bool> {
    let (a, b) = (read(studio)?, read(disk)?);
    Ok(same_children(
        &a,
        a.root().children(),
        &b,
        b.root().children(),
    ))
}

/// The differences between two model files. Unchanged instances are left out.
pub fn diff(studio: &[u8], disk: &[u8]) -> Result<Vec<Node>> {
    let (a, b) = (read(studio)?, read(disk)?);
    Ok(compare_children(
        &a,
        a.root().children(),
        &b,
        b.root().children(),
    ))
}

/// The top of one model, for when the other side has no file.
pub fn outline(bytes: &[u8], status: Status) -> Result<Vec<Node>> {
    let dom = read(bytes)?;
    Ok(dom
        .root()
        .children()
        .iter()
        .filter_map(|r| only(&dom, *r, status))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::{ustr, InstanceBuilder};

    fn model(build: impl FnOnce(&mut WeakDom, Ref)) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Model").with_name("Top"),
        );
        build(&mut dom, root);
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        bytes
    }

    #[test]
    fn equal_models_have_no_differences() {
        let bytes = model(|dom, root| {
            dom.insert(root, InstanceBuilder::new("Part").with_name("A"));
        });
        assert!(diff(&bytes, &bytes).unwrap().is_empty());
    }

    #[test]
    fn finds_changed_added_and_removed_instances() {
        let studio = model(|dom, root| {
            dom.insert(
                root,
                InstanceBuilder::new("StringValue")
                    .with_name("Same")
                    .with_property("Value", "new"),
            );
            dom.insert(root, InstanceBuilder::new("Part").with_name("OnlyHere"));
        });
        let disk = model(|dom, root| {
            dom.insert(
                root,
                InstanceBuilder::new("StringValue")
                    .with_name("Same")
                    .with_property("Value", "old"),
            );
            dom.insert(root, InstanceBuilder::new("Part").with_name("OnlyThere"));
        });
        let nodes = diff(&studio, &disk).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "Top");
        let children = &nodes[0].children;
        let find = |name: &str| children.iter().find(|n| n.name == name).unwrap();
        assert_eq!(find("Same").status, Status::Changed);
        assert_eq!(find("Same").properties[0].name, "Value");
        assert_eq!(find("Same").properties[0].studio.as_deref(), Some("new"));
        assert_eq!(find("Same").properties[0].disk.as_deref(), Some("old"));
        assert_eq!(find("OnlyHere").status, Status::OnlyStudio);
        assert_eq!(find("OnlyThere").status, Status::OnlyDisk);
    }

    #[test]
    fn repeated_names_pair_in_order() {
        let studio = model(|dom, root| {
            for _ in 0..2 {
                dom.insert(root, InstanceBuilder::new("Part").with_name("Dup"));
            }
        });
        let disk = model(|dom, root| {
            for _ in 0..3 {
                dom.insert(root, InstanceBuilder::new("Part").with_name("Dup"));
            }
        });
        let nodes = diff(&studio, &disk).unwrap();
        assert_eq!(nodes[0].children.len(), 1);
        assert_eq!(nodes[0].children[0].status, Status::OnlyDisk);
    }

    #[test]
    fn the_ustr_import_is_the_one_dom_uses() {
        assert_eq!(ustr("x").as_str(), "x");
    }

    #[test]
    fn a_file_that_leaves_out_defaults_has_the_same_content() {
        let studio = model(|dom, root| {
            dom.insert(
                root,
                InstanceBuilder::new("Part")
                    .with_name("Crate")
                    .with_property("Anchored", false),
            );
        });
        let file = model(|dom, root| {
            dom.insert(root, InstanceBuilder::new("Part").with_name("Crate"));
        });
        assert!(same_content(&studio, &file).unwrap());

        let anchored = model(|dom, root| {
            dom.insert(
                root,
                InstanceBuilder::new("Part")
                    .with_name("Crate")
                    .with_property("Anchored", true),
            );
        });
        assert!(!same_content(&anchored, &file).unwrap(), "not the default");

        let extra = model(|dom, root| {
            dom.insert(root, InstanceBuilder::new("Part").with_name("Crate"));
            dom.insert(root, InstanceBuilder::new("Part").with_name("Lid"));
        });
        assert!(!same_content(&extra, &file).unwrap(), "an extra instance");
    }
}

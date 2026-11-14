//! Two-way sync for the trees listed in a `twoway.json` beside the project.
//!
//! Rojo itself only syncs one way, from files to Studio. A project that also has a `twoway.json`
//! gets the twoway sync on the same server: those Studio trees and their folders are kept the same
//! in both directions, and conflicts are reviewed on a web page at `/twoway/`. Without the file,
//! nothing here runs and Rojo behaves as it always has.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use twoway::config::Config;
use twoway::service::Service;

use crate::project::ProjectNode;

pub const CONFIG_FILE: &str = "twoway.json";

/// Where the sync is mounted on Rojo's server.
pub const BASE: &str = "/twoway";

/// What the plugin learns about the sync from `/api/rojo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TwowayInfo {
    pub base: String,
    pub version: String,
    pub mappings: usize,
}

static INFO: OnceLock<TwowayInfo> = OnceLock::new();

/// The sync's details for `/api/rojo`, when it runs.
pub fn info() -> Option<TwowayInfo> {
    INFO.get().cloned()
}

/// True for a request the sync answers.
pub fn handles(path: &str) -> bool {
    path == BASE || path.starts_with(&format!("{BASE}/"))
}

/// Starts the sync if the project folder has a `twoway.json`.
pub fn start_if_configured(
    project_folder: &Path,
    tree: &ProjectNode,
    port: u16,
) -> anyhow::Result<Option<Arc<Service>>> {
    let config_path = project_folder.join(CONFIG_FILE);
    if !config_path.is_file() {
        return Ok(None);
    }
    let (config, root) = Config::load(&config_path)?;
    check_overlaps(tree, &config)
        .with_context(|| format!("{} can't be used", config_path.display()))?;

    let mappings = config.mappings.len();
    let page_url = format!("http://localhost:{port}{BASE}/");
    let service = Service::new(config, root, page_url, true)?;
    let _ = INFO.set(TwowayInfo {
        base: BASE.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        mappings,
    });
    Ok(Some(service))
}

fn is_within(path: &str, ancestor: &str) -> bool {
    path == ancestor || path.starts_with(&format!("{ancestor}/"))
}

/// Instance paths Rojo owns: nodes with a `$path`, which Rojo fills from files, and nodes that
/// delete what they don't know (`$ignoreUnknownInstances: false`).
fn rojo_owned(node: &ProjectNode, path: &str, owned: &mut Vec<(String, &'static str)>) {
    if !path.is_empty() {
        if node.path.is_some() {
            owned.push((path.to_string(), "has a $path"));
        } else if node.ignore_unknown_instances == Some(false) {
            owned.push((path.to_string(), "removes instances it doesn't know"));
        }
    }
    for (name, child) in &node.children {
        let child_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        rojo_owned(child, &child_path, owned);
    }
}

/// A two-way tree inside or around a tree Rojo fills from files would have both writing the same
/// instances, so that's refused up front.
fn check_overlaps(tree: &ProjectNode, config: &Config) -> anyhow::Result<()> {
    let mut owned = Vec::new();
    rojo_owned(tree, "", &mut owned);
    for mapping in &config.mappings {
        for (rojo_path, why) in &owned {
            let inside = is_within(&mapping.studio, rojo_path);
            let around = *why == "has a $path" && is_within(rojo_path, &mapping.studio);
            if inside || around {
                bail!(
                    "the mapping for {} overlaps {rojo_path} in the project, which {why}",
                    mapping.studio
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn tree() -> ProjectNode {
        serde_json::from_str(
            r#"{
                "$className": "DataModel",
                "ReplicatedStorage": {
                    "$className": "ReplicatedStorage",
                    "TS": { "$path": "out/shared" },
                    "Strict": { "$className": "Folder", "$ignoreUnknownInstances": false }
                }
            }"#,
        )
        .unwrap()
    }

    fn config(studio: &str) -> Config {
        serde_json::from_str(&format!(
            r#"{{ "mappings": [ {{ "studio": "{studio}", "path": "x" }} ] }}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_tree_next_to_rojo_trees_is_fine() {
        assert!(check_overlaps(&tree(), &config("ReplicatedStorage/GameData")).is_ok());
    }

    #[test]
    fn overlapping_rojo_trees_is_refused() {
        assert!(
            check_overlaps(&tree(), &config("ReplicatedStorage/TS/Thing")).is_err(),
            "inside a $path"
        );
        assert!(
            check_overlaps(&tree(), &config("ReplicatedStorage")).is_err(),
            "around a $path"
        );
        assert!(check_overlaps(&tree(), &config("ReplicatedStorage/Strict/Data")).is_err());
    }

    #[test]
    fn only_the_sync_s_own_paths_are_routed_to_it() {
        assert!(handles("/twoway"));
        assert!(handles("/twoway/api/status"));
        assert!(!handles("/twowayx"));
        assert!(!handles("/api/rojo"));
    }
}

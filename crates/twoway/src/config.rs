//! `twoway.json`: which Studio trees sync with which folders.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

pub use crate::modelfile::ModelFormat;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub mappings: Vec<Mapping>,
    #[serde(default)]
    pub ignore: Vec<String>,
    /// `port` and `autoConnect` were for the old standalone `twoway serve` and its plugin. Rojo serves the sync on
    /// its port and starts it with its Connect, so they're ignored, but still accepted so older
    /// files load.
    #[serde(default, rename = "port")]
    _port: Option<u16>,
    #[serde(default, rename = "autoConnect")]
    _auto_connect: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mapping {
    /// Instance path from the DataModel, such as `ReplicatedStorage/Data`.
    pub studio: String,
    /// Folder, relative to the config file.
    pub path: String,
    #[serde(default)]
    pub place_id: Option<u64>,
    #[serde(default)]
    pub place_name: Option<String>,
    /// Instance paths under `studio` that are each saved as one model file, scripts and all.
    #[serde(default)]
    pub models: Vec<String>,
    /// `auto`, the default, saves every instance that isn't a script or a Folder, without listing
    /// it. `listed` (also `manual` or `none`) saves only what `models` names.
    #[serde(default)]
    pub instances: InstanceMode,
    /// Instance paths under `studio` that never sync, in any mode. Patterns as in `models`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// How model files are written: `rbxmx` (the default) or `rbxm`. An existing file in the other
    /// format is converted the next time it syncs.
    #[serde(default)]
    pub model_format: ModelFormat,
    /// In auto mode, each folder's loose instances (not scripts or Folders) share one file,
    /// `init.rbxmx` in its directory. False gives every instance its own file.
    #[serde(default = "yes")]
    pub group_instances: bool,
    /// Folders whose loose instances get a file each, even with `groupInstances` on.
    #[serde(default)]
    pub separate: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceMode {
    #[serde(alias = "manual", alias = "none")]
    Listed,
    #[default]
    Auto,
}

impl Mapping {
    pub fn id(&self) -> String {
        format!("{}|{}", self.studio, self.path)
    }

    /// True when this place is named by the mapping. A mapping that names no place never matches
    /// here: it applies only when someone connects by hand, so opening an unrelated place can't
    /// sync it by accident.
    pub fn names_place(&self, place_id: u64, place_name: &str) -> bool {
        self.place_id == Some(place_id) || self.place_name.as_deref() == Some(place_name)
    }

    pub fn names_any_place(&self) -> bool {
        self.place_id.is_some() || self.place_name.is_some()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<(Self, PathBuf)> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let config: Config = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid", path.display()))?;
        if config.mappings.is_empty() {
            bail!("{} has no mappings", path.display());
        }
        let absolute = std::path::absolute(path)?;
        let root = absolute.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok((config, root))
    }

    pub fn ignore_set(&self) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.ignore {
            builder.add(Glob::new(pattern).with_context(|| format!("bad ignore glob {pattern}"))?);
        }
        Ok(builder.build()?)
    }

    /// The mappings a session in this place should use.
    /// The mappings for the open place: those that name it, and those that name no place.
    pub fn mappings_for(&self, place_id: u64, place_name: &str) -> Vec<Mapping> {
        self.mappings
            .iter()
            .filter(|m| m.names_place(place_id, place_name) || !m.names_any_place())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_str(
            r#"{ "mappings": [
                { "studio": "A", "path": "a" },
                { "studio": "B", "path": "b", "placeId": 7 },
                { "studio": "C", "path": "c", "placeName": "x.rbxl" }
            ] }"#,
        )
        .unwrap()
    }

    fn studios(list: Vec<Mapping>) -> Vec<String> {
        list.into_iter().map(|m| m.studio).collect()
    }

    #[test]
    fn unnamed_mappings_apply_in_any_place() {
        assert_eq!(studios(config().mappings_for(1, "other")), vec!["A"]);
    }

    #[test]
    fn settings_from_twoway_serve_still_load() {
        let config: Config =
            serde_json::from_str(r#"{ "port": 34900, "autoConnect": true, "mappings": [] }"#)
                .unwrap();
        assert!(config.mappings.is_empty());
    }

    #[test]
    fn instances_are_auto_unless_turned_off() {
        let parse = |extra: &str| -> Mapping {
            serde_json::from_str(&format!(r#"{{ "studio": "A", "path": "a"{extra} }}"#)).unwrap()
        };
        let plain = parse("");
        assert_eq!(plain.instances, InstanceMode::Auto);
        assert_eq!(plain.model_format, ModelFormat::Rbxmx);
        assert!(plain.group_instances);
        for off in ["listed", "manual", "none"] {
            assert_eq!(
                parse(&format!(r#", "instances": "{off}""#)).instances,
                InstanceMode::Listed
            );
        }
    }

    #[test]
    fn named_mappings_match_their_place_only() {
        assert_eq!(studios(config().mappings_for(7, "other")), vec!["A", "B"]);
        assert_eq!(studios(config().mappings_for(0, "x.rbxl")), vec!["A", "C"]);
    }
}

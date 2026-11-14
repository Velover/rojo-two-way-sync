//! `rojo studio`: work with an open Studio without a Connect.
//!
//! `list` names the open windows. `push` writes a project into one of them once, through Studio's
//! MCP server: Rojo's trees (made to match the files, as a Connect would), and the trees from a
//! `twoway.json`, with the rules of `twoway::push`: the files win where Studio is only behind, and
//! Studio work the sync has never seen is reported and left alone unless a side is chosen. A push
//! never writes the files or the sync's `.twoway/` state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail};
use clap::{Parser, Subcommand};
use memofs::Vfs;
use serde_json::{json, Value};
use twoway::push::{plan_mapping, MappingPlan, Side};

use crate::serve_session::ServeSession;
use crate::studio::{self, mcp::McpClient, Studio};
use crate::twoway_sync;

use super::resolve_path;

/// Work with an open Studio without connecting: list the windows, or push a project into one.
#[derive(Debug, Parser)]
pub struct StudioCommand {
    /// Print text for people instead of JSON.
    #[clap(long, global = true)]
    pub text: bool,

    #[clap(subcommand)]
    pub command: StudioSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum StudioSubcommand {
    /// The open Studio windows, with their ids and places.
    List,

    /// Writes the project into an open Studio once, without a Connect: Rojo's trees and the trees
    /// from a twoway.json. Never writes the files. Exits with 2 when conflicts were left alone.
    Push {
        /// Path to the project. Defaults to the current directory.
        #[clap(default_value = "")]
        project: PathBuf,

        /// The Studio: its id, its name, the place's name or id, or a unique part of its name.
        /// Needed when more than one is open.
        #[clap(long)]
        studio: Option<String>,

        /// Push only Rojo's trees, or only the two-way ones.
        #[clap(long, possible_values = &["rojo", "twoway"])]
        only: Option<String>,

        /// Settles a two-way conflict: PATH=disk sends the file, PATH=studio leaves Studio's.
        /// PATH is the instance's full path, as the conflict lists it. Repeatable.
        #[clap(long, value_name = "PATH=SIDE")]
        resolve: Vec<String>,

        /// Settles every two-way conflict the same way: disk or studio.
        #[clap(long, value_name = "SIDE")]
        resolve_all: Option<String>,

        /// Work out and report what would change, changing nothing.
        #[clap(long)]
        dry_run: bool,
    },
}

impl StudioCommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            StudioSubcommand::List => list(self.text),
            StudioSubcommand::Push {
                project,
                studio,
                only,
                resolve,
                resolve_all,
                dry_run,
            } => push(
                self.text,
                project,
                studio,
                only,
                resolve,
                resolve_all,
                dry_run,
            ),
        }
    }
}

fn print(text_mode: bool, value: &Value, describe: impl FnOnce(&Value) -> String) {
    if text_mode {
        println!("{}", describe(value));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    }
}

fn list(text_mode: bool) -> anyhow::Result<()> {
    let mut mcp = McpClient::connect()?;
    let windows = mcp.studios()?;
    let list: Vec<Value> = windows
        .iter()
        .map(|w| {
            json!({
                "id": w.id,
                "name": w.name,
                "placeId": w.place_id(),
                "file": w.file_name(),
            })
        })
        .collect();
    print(text_mode, &json!({ "studios": list }), |v| {
        let list = v["studios"].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return "No Studio is open.".to_string();
        }
        list.iter()
            .map(|w| {
                format!(
                    "{}  {}",
                    w["id"].as_str().unwrap_or(""),
                    w["name"]
                        .as_str()
                        .unwrap_or("(loading, or a test session's window)")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(())
}

/// The chosen sides, by full instance path, and for everything else.
struct Choices {
    by_path: BTreeMap<String, Side>,
    all: Option<Side>,
}

impl Choices {
    fn parse(resolve: &[String], resolve_all: Option<&str>) -> anyhow::Result<Self> {
        let mut by_path = BTreeMap::new();
        for entry in resolve {
            let (path, side) = entry
                .rsplit_once('=')
                .ok_or_else(|| anyhow!("--resolve takes PATH=disk or PATH=studio, not {entry}"))?;
            by_path.insert(path.trim_matches('/').to_string(), side.parse()?);
        }
        Ok(Choices {
            by_path,
            all: resolve_all.map(str::parse).transpose()?,
        })
    }

    fn side(&self, path: &str) -> Option<Side> {
        self.by_path.get(path).copied().or(self.all)
    }
}

#[allow(clippy::too_many_arguments)]
fn push(
    text_mode: bool,
    project: PathBuf,
    target: Option<String>,
    only: Option<String>,
    resolve: Vec<String>,
    resolve_all: Option<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let choices = Choices::parse(&resolve, resolve_all.as_deref())?;
    let project_path = resolve_path(&project)?;
    let vfs = Vfs::new_default()?;
    vfs.set_watch_enabled(false);
    let session = ServeSession::new(vfs, &project_path)?;
    let root_project = session.root_project();
    if session.tree().inner().root().class != "DataModel" {
        bail!("a push needs a place project: one whose tree is a DataModel");
    }
    let twoway_config =
        twoway_sync::load_config(root_project.folder_location(), &root_project.tree)?;

    let push_rojo = only.as_deref() != Some("twoway");
    let push_twoway = only.as_deref() != Some("rojo");
    if !push_twoway && (!choices.by_path.is_empty() || choices.all.is_some()) {
        bail!("--resolve is for the two-way trees, which --only rojo leaves out");
    }

    let mut studio = Studio::open(target.as_deref())?;
    let mut output = json!({
        "studio": { "id": studio.window.id, "name": studio.window.name },
        "dryRun": dry_run,
    });

    if push_rojo {
        let layout = twoway_sync::rojo_layout(root_project);
        let tree = session.tree();
        let report = studio.push_rojo(&tree, &layout, dry_run)?;
        output["rojo"] = serde_json::to_value(&report)?;
    }

    let mut conflicts_left = 0;
    if push_twoway {
        match &twoway_config {
            None if only.as_deref() == Some("twoway") => {
                bail!(
                    "{} has no {}",
                    root_project.folder_location().display(),
                    twoway_sync::CONFIG_FILE
                )
            }
            None => {}
            Some((config, root)) => {
                let window = studio.window.clone();
                let place_name = window
                    .file_name()
                    .or(window.place_name())
                    .unwrap_or("")
                    .to_string();
                let mappings = config.mappings_for(window.place_id().unwrap_or(0), &place_name);
                let mut reports = Vec::new();
                for (index, mapping) in mappings.iter().enumerate() {
                    let info = studio::mapping_info(index, mapping)?;
                    let read = studio.read_mapping(&info)?;
                    let mut plan: MappingPlan =
                        plan_mapping(index, mapping, config, root, read.side, &|path| {
                            choices.side(path)
                        })?;
                    plan.warnings.extend(read.warnings);
                    conflicts_left += plan.conflicts.len();

                    let mut failed = Vec::new();
                    let mut kept_in_studio = Vec::new();
                    if !dry_run && !plan.ops.is_empty() {
                        let (results, warnings) = studio.apply_ops(&info, &plan.ops)?;
                        plan.warnings.extend(warnings);
                        for planned in &plan.ops {
                            match results.get(&planned.op.id) {
                                Some(result) if result.ok => {}
                                Some(result) if result.kept => {
                                    kept_in_studio.push(planned.path.clone())
                                }
                                Some(result) => failed.push(json!({
                                    "path": planned.path,
                                    "why": result.error.clone().unwrap_or_else(|| {
                                        "Studio's text changed while pushing; left alone".into()
                                    }),
                                })),
                                None => failed.push(
                                    json!({ "path": planned.path, "why": "no answer from Studio" }),
                                ),
                            }
                        }
                    }
                    let count =
                        |change: &str| plan.ops.iter().filter(|o| o.change == change).count();
                    reports.push(json!({
                        "studio": mapping.studio,
                        "foundInStudio": read.found,
                        "created": count("create"),
                        "updated": count("update"),
                        "deleted": count("delete"),
                        "changes": plan.ops.iter().map(|o| json!({ "path": o.path, "change": o.change })).collect::<Vec<_>>(),
                        "failed": failed,
                        "keptBecauseUnsynced": kept_in_studio,
                        "conflicts": plan.conflicts,
                        "keptStudio": plan.kept,
                        "studioOnly": plan.studio_only,
                        "skipped": plan.skipped.iter().map(|(p, why)| json!({ "path": p, "why": why })).collect::<Vec<_>>(),
                        "warnings": plan.warnings,
                    }));
                }
                output["twoway"] = json!({ "mappings": reports });
            }
        }
    }

    print(text_mode, &output, describe);
    if conflicts_left > 0 {
        // Printed above; the exit code tells a script that something waits for a decision.
        std::process::exit(2);
    }
    Ok(())
}

fn describe(v: &Value) -> String {
    let mut lines = Vec::new();
    let verb = if v["dryRun"] == true {
        "Would push"
    } else {
        "Pushed"
    };
    lines.push(format!(
        "{verb} into {}:",
        v["studio"]["name"].as_str().unwrap_or("Studio")
    ));
    if let Some(rojo) = v.get("rojo") {
        lines.push(format!(
            "  Rojo's trees ({}): {} created, {} updated, {} replaced, {} removed, {} unchanged",
            rojo["trees"].as_array().map_or(0, Vec::len),
            rojo["created"],
            rojo["updated"],
            rojo["replaced"],
            rojo["removed"],
            rojo["unchanged"]
        ));
        for error in rojo["errors"].as_array().into_iter().flatten() {
            lines.push(format!("    error: {}", error.as_str().unwrap_or("")));
        }
    }
    for mapping in v["twoway"]["mappings"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {}: {} created, {} updated, {} deleted",
            mapping["studio"].as_str().unwrap_or(""),
            mapping["created"],
            mapping["updated"],
            mapping["deleted"]
        ));
        for failed in mapping["failed"].as_array().into_iter().flatten() {
            lines.push(format!(
                "    failed {}: {}",
                failed["path"].as_str().unwrap_or(""),
                failed["why"].as_str().unwrap_or("")
            ));
        }
        for conflict in mapping["conflicts"].as_array().into_iter().flatten() {
            let mut line = format!(
                "    conflict {}: {}",
                conflict["path"].as_str().unwrap_or(""),
                conflict["reason"].as_str().unwrap_or("")
            );
            if let Some(side) = conflict["suggested"].as_str() {
                line.push_str(&format!(" (suggested: {side})"));
            }
            lines.push(line);
        }
        for path in mapping["keptStudio"].as_array().into_iter().flatten() {
            lines.push(format!("    kept Studio's {}", path.as_str().unwrap_or("")));
        }
        let studio_only = mapping["studioOnly"].as_array().map_or(0, Vec::len);
        if studio_only > 0 {
            lines.push(format!(
                "    {studio_only} instance(s) only Studio has, left alone"
            ));
        }
        for warning in mapping["warnings"].as_array().into_iter().flatten() {
            lines.push(format!("    warning: {}", warning.as_str().unwrap_or("")));
        }
    }
    let conflicts: usize = v["twoway"]["mappings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| m["conflicts"].as_array().map_or(0, Vec::len))
        .sum();
    if conflicts > 0 {
        lines.push(format!(
            "{conflicts} conflict(s) left alone. Settle them with --resolve PATH=disk|studio, or --resolve-all."
        ));
    }
    lines.join("\n")
}

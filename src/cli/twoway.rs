//! `rojo twoway`: the two-way sync from the command line, for people and for agents.
//!
//! Conflicts only exist while `rojo serve` runs, so `status`, `conflicts`, `show` and `resolve`
//! ask the running server, through the same API as the review page. `history` and `restore` read
//! `.twoway/` directly and work either way. `check` and `build` work on saved place files and
//! need no server or Studio.
//!
//! Output is JSON, for agents; `--text` prints it for people instead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::{Parser, Subcommand};
use memofs::Vfs;
use serde_json::{json, Value};
use twoway::config::Config;
use twoway::engine::{find_blob, read_history, HistoryEntry};
use twoway::modelfile::{self, ModelFormat};
use twoway::place_build::{self, BuildReport};

use crate::project::Project;
use crate::serve_session::ServeSession;
use crate::twoway_sync;

use super::resolve_path;

/// `rojo serve`'s port when neither the project nor `--port` names one.
const DEFAULT_PORT: u16 = 34872;

/// Look after the two-way sync from a `twoway.json` beside the project.
#[derive(Debug, Parser)]
pub struct TwowayCommand {
    /// Path to the project. Defaults to the current directory.
    #[clap(long, default_value = "")]
    pub project: PathBuf,

    /// The port `rojo serve` runs on, if it isn't the project's `servePort` or 34872.
    #[clap(long)]
    pub port: Option<u16>,

    /// Print text for people instead of JSON.
    #[clap(long, global = true)]
    pub text: bool,

    #[clap(subcommand)]
    pub command: TwowaySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum TwowaySubcommand {
    /// Whether the sync is running, which place is connected, conflicts and warnings.
    Status,

    /// The items waiting for a decision, each with the token `resolve` takes.
    Conflicts,

    /// One conflict in full: Studio's and the disk's versions, the last agreed one, and a diff.
    Show {
        /// The conflict's number, from `conflicts`.
        id: u64,
    },

    /// Settles a conflict by keeping one side, or by writing a merged version.
    Resolve {
        /// The conflict's number, from `conflicts`.
        id: u64,

        /// The side to keep: `studio` or `disk`.
        #[clap(long, conflicts_with = "merged", required_unless_present = "merged")]
        keep: Option<String>,

        /// A file holding the merged text, for a script both sides edited.
        #[clap(long)]
        merged: Option<PathBuf>,

        /// The token `conflicts` or `show` printed. When given, the conflict is only settled if
        /// neither side changed since.
        #[clap(long)]
        token: Option<String>,
    },

    /// Recent syncs, oldest first.
    History {
        /// How many entries.
        #[clap(long, default_value = "30")]
        limit: usize,

        /// Only entries whose instance path contains this.
        #[clap(long)]
        path: Option<String>,
    },

    /// Writes a stored version, as `history` lists them, back to a file in a mapped folder. A
    /// running sync then sends it to Studio like any other edit.
    Restore {
        /// The version: the start of its hash, at least 6 digits.
        version: String,

        /// The file to write.
        file: PathBuf,
    },

    /// Compares the mapped folders with a saved place and says what a first sync would do.
    /// Writes nothing.
    Check {
        /// The saved place to compare with.
        #[clap(long)]
        place: PathBuf,

        /// Include mappings that name this place id.
        #[clap(long)]
        place_id: Option<u64>,
    },

    /// Builds a place from a saved one, so its map and everything else stay: the project's trees
    /// replace theirs, then the mapped folders' files go in. `rojo build` makes a place from the
    /// project alone, with the mapped files too.
    Build {
        /// The place to start from. It's only read.
        #[clap(long)]
        base: PathBuf,

        /// Where to write the place (.rbxl or .rbxlx).
        #[clap(long, short)]
        output: PathBuf,

        /// Include mappings that name this place id. Defaults to the project's `placeId`.
        #[clap(long)]
        place_id: Option<u64>,
    },
}

impl TwowayCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project)?;
        let vfs = Vfs::new_default()?;
        vfs.set_watch_enabled(false);
        let project = Project::load_fuzzy(&vfs, &project_path)?
            .ok_or_else(|| anyhow!("no Rojo project at {}", project_path.display()))?;
        let (config, root) = twoway_sync::load_config(project.folder_location(), &project.tree)?
            .ok_or_else(|| {
                anyhow!(
                    "{} has no {}, so it has no two-way sync",
                    project.folder_location().display(),
                    twoway_sync::CONFIG_FILE
                )
            })?;
        let port = self.port.or(project.serve_port).unwrap_or(DEFAULT_PORT);
        let server = Server {
            port,
            project: &project,
        };
        let out = Output { text: self.text };

        match self.command {
            TwowaySubcommand::Status => status(&server, &root, &out),
            TwowaySubcommand::Conflicts => conflicts(&server, &out),
            TwowaySubcommand::Show { id } => show(&server, id, &out),
            TwowaySubcommand::Resolve {
                id,
                keep,
                merged,
                token,
            } => resolve(&server, id, keep, merged, token, &out),
            TwowaySubcommand::History { limit, path } => history(&root, limit, path, &out),
            TwowaySubcommand::Restore { version, file } => {
                restore(&server, &config, &root, &version, &file, &out)
            }
            TwowaySubcommand::Check { place, place_id } => twoway::check::check(
                &project.folder_location().join(twoway_sync::CONFIG_FILE),
                &place,
                place_id,
            ),
            TwowaySubcommand::Build {
                base,
                output,
                place_id,
            } => build(
                &project_path,
                &project,
                &config,
                &root,
                &base,
                &output,
                place_id,
                &out,
            ),
        }
    }
}

struct Output {
    text: bool,
}

impl Output {
    /// JSON, or the text `describe` makes of it.
    fn print(&self, value: &Value, describe: impl FnOnce(&Value) -> String) {
        if self.text {
            println!("{}", describe(value));
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
        }
    }
}

/// The `rojo serve` running this project.
struct Server<'a> {
    port: u16,
    project: &'a Project,
}

impl Server<'_> {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn client() -> anyhow::Result<reqwest::blocking::Client> {
        Ok(reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?)
    }

    /// Checks that the server on the port is Rojo serving this project with the two-way sync.
    /// `Ok(None)` when nothing answers.
    fn find(&self) -> anyhow::Result<Option<()>> {
        let response = match Self::client()?.get(self.url("/api/rojo")).send() {
            Ok(response) => response,
            Err(error) if error.is_connect() || error.is_timeout() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // Rojo's own API speaks MessagePack; only the two fields that matter here are read.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Info {
            project_name: String,
            #[serde(default)]
            twoway: Option<serde::de::IgnoredAny>,
        }
        let bytes = response.bytes()?;
        let info: Info = serde::Deserialize::deserialize(
            &mut rmp_serde::Deserializer::new(&bytes[..]).with_human_readable(),
        )
        .with_context(|| format!("port {} doesn't answer like Rojo", self.port))?;
        if let Some(name) = &self.project.name {
            if info.project_name != *name {
                bail!(
                    "the Rojo on port {} serves '{}', not '{name}'; pass --port",
                    self.port,
                    info.project_name
                );
            }
        }
        if info.twoway.is_none() {
            bail!(
                "the Rojo on port {} has no two-way sync: it's stock Rojo, or it started before {} existed",
                self.port,
                twoway_sync::CONFIG_FILE
            );
        }
        Ok(Some(()))
    }

    /// Like `find`, for commands that need the server.
    fn require(&self) -> anyhow::Result<()> {
        self.find()?.ok_or_else(|| {
            anyhow!(
                "Rojo isn't serving this project on port {}. Conflicts only exist while `rojo serve` runs.",
                self.port
            )
        })
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        let response = Self::client()?
            .get(self.url(&format!("{}{path}", twoway_sync::BASE)))
            .send()?;
        Self::read(response)
    }

    fn post(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let response = Self::client()?
            .post(self.url(&format!("{}{path}", twoway_sync::BASE)))
            .json(body)
            .send()?;
        Self::read(response)
    }

    fn read(response: reqwest::blocking::Response) -> anyhow::Result<Value> {
        let status = response.status();
        let value: Value = response.json().unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "{}",
                value["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("the server answered {status}"))
            );
        }
        Ok(value)
    }

    fn conflicts(&self) -> anyhow::Result<Vec<Value>> {
        let value = self.get("/api/conflicts")?;
        Ok(value["conflicts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(summarize)
            .collect())
    }

    fn conflict(&self, id: u64) -> anyhow::Result<Value> {
        self.conflicts()?
            .into_iter()
            .find(|c| c["id"].as_u64() == Some(id))
            .ok_or_else(|| anyhow!("there's no conflict {id}; `rojo twoway conflicts` lists them"))
    }
}

fn hash_of(side: &Value) -> Option<&str> {
    side["hash"].as_str()
}

/// Both sides' hashes in one string: a conflict whose token changed has a side that moved on.
fn token_of(conflict: &Value) -> String {
    format!(
        "{}:{}",
        hash_of(&conflict["studio"]).unwrap_or("none"),
        hash_of(&conflict["disk"]).unwrap_or("none")
    )
}

/// A conflict as the commands print it: where it is, why, and its token.
fn summarize(conflict: Value) -> Value {
    let key = conflict["key"].as_str().unwrap_or("");
    let root = conflict["studioRoot"].as_str().unwrap_or("");
    let path = if key.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{key}")
    };
    let side = |side: &Value| {
        if side.is_null() {
            Value::Null
        } else {
            json!({ "kind": side["kind"], "hash": side["hash"] })
        }
    };
    json!({
        "id": conflict["id"],
        "path": path,
        "file": conflict["diskPath"],
        "reason": conflict["reason"],
        "suggested": conflict["suggested"],
        "studio": side(&conflict["studio"]),
        "disk": side(&conflict["disk"]),
        "token": token_of(&conflict),
    })
}

fn describe_side(side: &Value) -> String {
    if side.is_null() {
        "missing".to_string()
    } else {
        side["kind"].as_str().unwrap_or("?").to_string()
    }
}

fn describe_conflict(conflict: &Value) -> String {
    let mut line = format!(
        "#{} {}: {} (Studio: {}, disk: {})",
        conflict["id"],
        conflict["path"].as_str().unwrap_or(""),
        conflict["reason"].as_str().unwrap_or(""),
        describe_side(&conflict["studio"]),
        describe_side(&conflict["disk"]),
    );
    if let Some(suggested) = conflict["suggested"].as_str() {
        line.push_str(&format!(", suggested: keep {suggested}"));
    }
    line
}

fn status(server: &Server, root: &Path, out: &Output) -> anyhow::Result<()> {
    let value = match server.find()? {
        Some(()) => {
            let status = server.get("/api/status")?;
            json!({
                "running": true,
                "port": server.port,
                "connectedPlace": status["connectedPlace"],
                "conflicts": status["conflicts"],
                "warnings": status["warnings"],
            })
        }
        None => json!({
            "running": false,
            "port": server.port,
            "lastSync": read_history(&root.join(".twoway"), 1).pop().map(|e| e.at),
        }),
    };
    out.print(&value, |v| {
        if v["running"] == true {
            let mut text = format!(
                "Rojo serves the two-way sync on port {}. Connected place: {}. Conflicts: {}.",
                v["port"],
                v["connectedPlace"].as_str().unwrap_or("none"),
                v["conflicts"]
            );
            for warning in v["warnings"].as_array().into_iter().flatten() {
                text.push_str(&format!("\nwarning: {}", warning.as_str().unwrap_or("")));
            }
            text
        } else {
            format!("Rojo isn't serving this project on port {}.", v["port"])
        }
    });
    Ok(())
}

fn conflicts(server: &Server, out: &Output) -> anyhow::Result<()> {
    server.require()?;
    let list = server.conflicts()?;
    out.print(&json!({ "conflicts": list }), |v| {
        let list = v["conflicts"].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            "No conflicts.".to_string()
        } else {
            list.iter()
                .map(describe_conflict)
                .collect::<Vec<_>>()
                .join("\n")
        }
    });
    Ok(())
}

fn show(server: &Server, id: u64, out: &Output) -> anyhow::Result<()> {
    server.require()?;
    let conflict = server.conflict(id)?;
    let detail = server.get(&format!("/api/conflicts/{id}"))?;
    let value = json!({ "conflict": conflict, "detail": detail });
    out.print(&value, |v| {
        let mut text = describe_conflict(&v["conflict"]);
        text.push_str(&format!(
            "\ntoken: {}",
            v["conflict"]["token"].as_str().unwrap_or("")
        ));
        let detail = &v["detail"];
        if let Some(diff) = detail["diff"].as_str() {
            text.push_str("\n\n");
            text.push_str(diff);
        } else if let Some(texts) = detail["texts"].as_object() {
            for side in ["studio", "disk", "base"] {
                if let Some(body) = texts.get(side).and_then(Value::as_str) {
                    text.push_str(&format!("\n\n--- {side} ---\n{body}"));
                }
            }
        } else if detail["model"].is_array() {
            text.push_str("\n\nA model: run without --text for its instance tree.");
        }
        text
    });
    Ok(())
}

fn resolve(
    server: &Server,
    id: u64,
    keep: Option<String>,
    merged: Option<PathBuf>,
    token: Option<String>,
    out: &Output,
) -> anyhow::Result<()> {
    server.require()?;
    let conflict = server.conflict(id)?;
    if let Some(token) = &token {
        if *token != conflict["token"] {
            bail!("conflict {id} changed since that token was printed; run `rojo twoway show {id}` again");
        }
    }
    let (choice, text) = match (keep.as_deref(), &merged) {
        (Some("studio"), None) => ("studio", None),
        (Some("disk"), None) => ("disk", None),
        (Some(other), None) => bail!("--keep is studio or disk, not {other}"),
        (None, Some(file)) => (
            "merged",
            Some(
                fs_err::read_to_string(file)
                    .with_context(|| format!("couldn't read the merge from {}", file.display()))?,
            ),
        ),
        _ => bail!("give --keep or --merged"),
    };
    if text
        .as_deref()
        .is_some_and(|t| t.contains("<<<<<<<") || t.contains(">>>>>>>"))
    {
        bail!("the merge still has conflict markers");
    }
    server.post(
        &format!("/api/conflicts/{id}/resolve"),
        &json!({
            "choice": choice,
            "text": text,
            "studioHash": hash_of(&conflict["studio"]),
            "diskHash": hash_of(&conflict["disk"]),
        }),
    )?;
    out.print(
        &json!({ "resolved": id, "kept": choice, "path": conflict["path"] }),
        |v| {
            format!(
                "Settled #{id} ({}) with {}.",
                v["path"].as_str().unwrap_or(""),
                if choice == "merged" {
                    "the merge".to_string()
                } else {
                    format!("{choice}'s version")
                }
            )
        },
    );
    Ok(())
}

fn history(root: &Path, limit: usize, path: Option<String>, out: &Output) -> anyhow::Result<()> {
    let dir = root.join(".twoway");
    // Filtering after reading the last `limit` would return fewer than asked, so read them all.
    let entries: Vec<HistoryEntry> = read_history(&dir, usize::MAX)
        .into_iter()
        .filter(|entry| {
            path.as_deref()
                .is_none_or(|p| instance_path(entry).contains(p))
        })
        .collect();
    let entries = &entries[entries.len().saturating_sub(limit)..];
    let list: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "at": entry.at,
                "path": instance_path(entry),
                "direction": entry.direction,
                "change": entry.change,
                "from": entry.from,
                "to": entry.to,
            })
        })
        .collect();
    out.print(&json!({ "history": list }), |v| {
        let list = v["history"].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return "No history yet.".to_string();
        }
        list.iter()
            .map(|e| {
                let short = |h: &Value| h.as_str().map(|h| h[..h.len().min(10)].to_string());
                format!(
                    "{} {} {} {}{}",
                    e["at"].as_u64().map(time_of).unwrap_or_default(),
                    if e["direction"] == "toDisk" {
                        "Studio -> disk"
                    } else {
                        "disk -> Studio"
                    },
                    e["change"].as_str().unwrap_or(""),
                    e["path"].as_str().unwrap_or(""),
                    short(&e["to"])
                        .map(|h| format!(" (version {h})"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(())
}

/// A history time, in milliseconds since 1970, as text (UTC).
fn time_of(at: u64) -> String {
    humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + Duration::from_millis(at)).to_string()
}

/// The instance path a history entry is about. Its mapping is stored as `studio|path`.
fn instance_path(entry: &HistoryEntry) -> String {
    let studio = entry.mapping.split('|').next().unwrap_or("");
    if entry.key.is_empty() {
        studio.to_string()
    } else {
        format!("{studio}/{}", entry.key)
    }
}

fn restore(
    server: &Server,
    config: &Config,
    root: &Path,
    version: &str,
    file: &Path,
    out: &Output,
) -> anyhow::Result<()> {
    let target = std::env::current_dir()?.join(file);
    let inside = config.mappings.iter().any(|m| {
        let folder = root.join(&m.path);
        target.starts_with(&folder)
            || dunce::canonicalize(&folder).is_ok_and(|f| {
                target
                    .parent()
                    .and_then(|p| dunce::canonicalize(p).ok())
                    .is_some_and(|p| p.starts_with(f))
            })
    });
    if !inside {
        bail!(
            "{} isn't in a folder {} maps",
            file.display(),
            twoway_sync::CONFIG_FILE
        );
    }

    let dir = root.join(".twoway");
    let hash = find_blob(&dir, version)?;
    let mut bytes = fs_err::read(dir.join("blobs").join(&hash))?;
    // Models are stored in either format; the file's extension says which it needs.
    let format = match target.extension().and_then(|e| e.to_str()) {
        Some("rbxm") => Some(ModelFormat::Rbxm),
        Some("rbxmx") => Some(ModelFormat::Rbxmx),
        _ => None,
    };
    if let Some(format) = format {
        bytes = modelfile::convert(&bytes, format)?;
    }
    if let Some(parent) = target.parent() {
        fs_err::create_dir_all(parent)?;
    }
    fs_err::write(&target, &bytes)?;

    let running = server.find().ok().flatten().is_some();
    out.print(
        &json!({ "restored": hash, "file": target, "syncing": running }),
        |_| {
            format!(
                "Wrote version {} to {}. {}",
                &hash[..10.min(hash.len())],
                file.display(),
                if running {
                    "The running sync sends it to Studio."
                } else {
                    "It goes to Studio on the next Connect."
                }
            )
        },
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build(
    project_path: &Path,
    project: &Project,
    config: &Config,
    root: &Path,
    base: &Path,
    output: &Path,
    place_id: Option<u64>,
    out: &Output,
) -> anyhow::Result<()> {
    if !matches!(
        output.extension().and_then(|e| e.to_str()),
        Some("rbxl" | "rbxlx")
    ) {
        bail!("the output is a place file: .rbxl or .rbxlx");
    }
    let mut dom = place_build::read_place(base)?;

    let vfs = Vfs::new_default()?;
    vfs.set_watch_enabled(false);
    let session = ServeSession::new(vfs, project_path)?;
    let mut built = twoway_sync::copy_place(session.tree().inner());
    // Dropping the session can take long and we're done with it.
    std::mem::forget(session);

    let mut report = BuildReport::default();
    place_build::lay_rojo(
        &mut dom,
        &mut built,
        &twoway_sync::rojo_layout(project),
        &mut report,
    )?;
    let mappings = place_build::mappings_for_place(
        config,
        place_id.or(project.place_id),
        place_build::place_file_name(base),
    );
    place_build::lay_mappings(&mut dom, config, root, &mappings, &mut report)?;
    place_build::write_place(output, &dom)?;

    out.print(
        &json!({
            "output": output,
            "rojoTrees": report.rojo_trees,
            "twoWayTrees": mappings.len(),
            "scripts": report.scripts,
            "models": report.models,
            "removed": report.removed,
            "repairedReferences": report.repaired_references,
            "lostReferences": report.lost_references,
            "kept": report.kept,
        }),
        |v| {
            let mut text = format!(
                "Wrote {}: {} Rojo tree(s), {} two-way tree(s) with {} script(s) and {} model file(s), {} removed.",
                output.display(),
                v["rojoTrees"],
                v["twoWayTrees"],
                v["scripts"],
                v["models"],
                v["removed"]
            );
            for lost in v["lostReferences"].as_array().into_iter().flatten() {
                text.push_str(&format!(
                    "\nwarning: {} pointed into a replaced tree and was cleared",
                    lost.as_str().unwrap_or("")
                ));
            }
            for kept in v["kept"].as_array().into_iter().flatten() {
                text.push_str(&format!(
                    "\nkept {}: its file is gone, but it holds instances that aren't synced",
                    kept.as_str().unwrap_or("")
                ));
            }
            text
        },
    );
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_conflict_is_summarized_with_its_path_and_token() {
        let conflict = json!({
            "id": 3,
            "studioRoot": "ReplicatedStorage/GameData",
            "key": "Inventory",
            "reason": "both changed",
            "suggested": null,
            "studio": { "kind": "script", "hash": "aaa" },
            "disk": null,
            "diskPath": "StudioSynced/GameData/Inventory.luau",
        });
        let summary = summarize(conflict);
        assert_eq!(summary["path"], "ReplicatedStorage/GameData/Inventory");
        assert_eq!(summary["token"], "aaa:none");
        assert_eq!(summary["disk"], Value::Null);
    }

    #[test]
    fn history_entries_name_their_instance() {
        let entry = HistoryEntry {
            at: 0,
            mapping: "ReplicatedStorage/GameData|StudioSynced/GameData".into(),
            key: "Inventory".into(),
            direction: "toDisk".into(),
            change: "update".into(),
            from: None,
            to: None,
        };
        assert_eq!(
            instance_path(&entry),
            "ReplicatedStorage/GameData/Inventory"
        );
    }
}

//! Running things in an open Studio without a Connect, for `rojo studio`: list the windows, and
//! push a project's trees (Rojo's and the two-way ones) into one of them once.
//!
//! Everything goes through Studio's MCP server and its `execute_luau` tool. Luau for the work is
//! sent as text: the plugin's own `ScriptText.lua` and `TwoWayStudio.lua`, inlined, so a push
//! changes Studio exactly as the plugin would. Each call is one undo step in Studio.

pub mod mcp;
pub mod queue;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use data_encoding::BASE64;
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use serde::Serialize;
use serde_json::{json, Value};
use twoway::config::Mapping;
use twoway::engine::StudioEntry;
use twoway::place_build::RojoLayout;
use twoway::push::{PlannedOp, StudioSide};
use twoway::tree::ModelRule;

use crate::snapshot::RojoTree;

use self::mcp::{McpClient, StudioWindow};
use self::queue::ExecTurn;

const SCRIPT_TEXT: &str = include_str!("../../plugin/src/ScriptText.lua");
const TWOWAY_STUDIO: &str = include_str!("../../plugin/src/TwoWayStudio.lua");
const COMMON: &str = include_str!("common.luau");
const TWOWAY_SNAPSHOT: &str = include_str!("twoway_snapshot.luau");
const TWOWAY_APPLY: &str = include_str!("twoway_apply.luau");
const ROJO_PUSH: &str = include_str!("rojo_push.luau");

/// The line in TwoWayStudio.lua that the inlined copy replaces.
const SCRIPT_TEXT_REQUIRE: &str = "local ScriptText = require(script.Parent.ScriptText)";

/// How much of a long answer one call returns. `execute_luau` answers are cut at about 100,000
/// characters, and base64 makes 60,000 bytes into 80,000.
const ANSWER_CHUNK_BYTES: usize = 60_000;

/// How much one call sends. Studio has taken calls of 4 MB; this stays well under.
const CALL_BYTES: usize = 1_500_000;

const CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// The modules as one Luau prelude, each in a function so its locals stay its own.
fn modules() -> String {
    assert!(
        TWOWAY_STUDIO.contains(SCRIPT_TEXT_REQUIRE),
        "TwoWayStudio.lua no longer requires ScriptText the way it's inlined"
    );
    let twoway = TWOWAY_STUDIO.replacen(SCRIPT_TEXT_REQUIRE, "local ScriptText = ScriptText", 1);
    format!(
        "local ScriptText = (function()\n{SCRIPT_TEXT}\nend)()\nlocal TwoWayStudio = (function()\n{twoway}\nend)()\n"
    )
}

/// A Luau long string holding `text` exactly: its brackets get enough `=` that the text can't
/// close them.
fn long_string(text: &str) -> String {
    let mut level = 0;
    while text.contains(&format!("]{}]", "=".repeat(level))) {
        level += 1;
    }
    let equals = "=".repeat(level);
    // A newline right after the opening bracket would be dropped, so one is always there.
    format!("[{equals}[\n{text}]{equals}]")
}

/// The whole snippet for a driver, with its input.
fn snippet(driver: &str, input: &Value) -> String {
    let common = COMMON.replacen("--[[INPUT]]", &long_string(&input.to_string()), 1);
    format!("{}\n{common}\n{driver}", modules())
}

/// FNV-1a, 32 bits, as `common.luau` computes it.
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(16_777_619)
    })
}

/// What's between `<<` and `>>` in an answer.
fn framed(text: &str) -> anyhow::Result<&str> {
    let start = text
        .find("<<")
        .ok_or_else(|| anyhow!("unexpected answer from Studio: {}", preview(text)))?;
    let end = text[start..]
        .find(">>")
        .ok_or_else(|| anyhow!("unexpected answer from Studio: {}", preview(text)))?;
    Ok(&text[start + 2..start + end])
}

fn preview(text: &str) -> String {
    text.chars().take(300).collect()
}

/// Studio's error text without the Assistant's own locations in front of it.
fn clean_error(text: &str) -> String {
    let mut rest = text.trim();
    while let Some(colon) = rest.find(": ") {
        let head = &rest[..colon];
        let is_location = (head.starts_with("sabuiltin_") || head.starts_with("AssistantCommand"))
            && head
                .rsplit(':')
                .next()
                .is_some_and(|n| n.parse::<u32>().is_ok());
        if !is_location {
            break;
        }
        rest = &rest[colon + 2..];
    }
    rest.to_string()
}

/// Picks the window a target names: its id, its name, the place's name or id, or a unique part
/// of its name. With no target, the only window there is.
pub fn pick_window(windows: &[StudioWindow], target: Option<&str>) -> anyhow::Result<StudioWindow> {
    let named: Vec<&StudioWindow> = windows.iter().filter(|w| w.name.is_some()).collect();
    let list = || {
        if named.is_empty() {
            "none".to_string()
        } else {
            named
                .iter()
                .map(|w| w.name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let Some(target) = target else {
        return match named.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no Studio window is open (or it's still loading)"),
            _ => bail!(
                "several Studios are open, so name one with --studio: {}",
                list()
            ),
        };
    };
    let exact: Vec<&&StudioWindow> = named
        .iter()
        .filter(|w| {
            w.id == target
                || w.name.as_deref() == Some(target)
                || w.place_name() == Some(target)
                || w.place_id().is_some_and(|id| id.to_string() == target)
        })
        .collect();
    if let [one] = exact.as_slice() {
        return Ok((**one).clone());
    }
    let lower = target.to_lowercase();
    let partial: Vec<&&StudioWindow> = named
        .iter()
        .filter(|w| {
            w.name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains(&lower))
        })
        .collect();
    match partial.as_slice() {
        [one] => Ok((**one).clone()),
        [] => bail!("no Studio matches \"{target}\" (open: {})", list()),
        _ => bail!(
            "several Studios match \"{target}\": {}",
            partial
                .iter()
                .map(|w| w.name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// One Studio window, ready for calls.
pub struct Studio {
    mcp: McpClient,
    pub window: StudioWindow,
}

impl Studio {
    pub fn open(target: Option<&str>) -> anyhow::Result<Self> {
        let mut mcp = McpClient::connect()?;
        let windows = mcp.studios()?;
        let window = pick_window(&windows, target)?;
        Ok(Studio { mcp, window })
    }

    /// Runs a snippet and returns its framed answer.
    fn run(&mut self, code: &str) -> anyhow::Result<String> {
        let _turn = ExecTurn::take("rojo studio push")?;
        let (text, is_error) = self.mcp.call_tool(
            "execute_luau",
            json!({ "code": code, "datamodel_type": "Edit", "studio_id": self.window.id }),
            CALL_TIMEOUT,
        )?;
        if is_error || text.contains("AssistantCommand:") {
            bail!("Studio: {}", clean_error(&text));
        }
        Ok(framed(&text)?.to_string())
    }

    fn run_json(&mut self, code: &str) -> anyhow::Result<Value> {
        let framed = self.run(code)?;
        let bytes = BASE64
            .decode(framed.as_bytes())
            .context("Studio's answer isn't base64")?;
        serde_json::from_slice(&bytes).context("Studio's answer isn't JSON")
    }

    /// Reads one mapped tree from Studio, in as many calls as it takes.
    pub fn read_mapping(&mut self, info: &Value) -> anyhow::Result<StudioRead> {
        'attempt: for _ in 0..3 {
            let mut payload = Vec::new();
            let mut expected: Option<(usize, u32)> = None;
            let mut chunk = 0;
            loop {
                let input =
                    json!({ "mapping": info, "chunk": chunk, "chunkBytes": ANSWER_CHUNK_BYTES });
                let answer = self.run(&snippet(TWOWAY_SNAPSHOT, &input))?;
                let mut parts = answer.splitn(3, '|');
                let total: usize = parts.next().unwrap_or("").parse()?;
                let sum: u32 = parts.next().unwrap_or("").parse()?;
                let piece = BASE64.decode(parts.next().unwrap_or("").as_bytes())?;
                match expected {
                    None => expected = Some((total, sum)),
                    // Studio changed between calls: start over.
                    Some(previous) if previous != (total, sum) => continue 'attempt,
                    Some(_) => {}
                }
                payload.extend_from_slice(&piece);
                chunk += 1;
                if payload.len() >= total || piece.is_empty() {
                    break;
                }
            }
            let (total, sum) = expected.unwrap_or_default();
            if payload.len() != total || checksum(&payload) != sum {
                continue;
            }
            return StudioRead::parse(&payload);
        }
        bail!("Studio's tree kept changing while it was read; try again when nobody is editing it")
    }

    /// Carries out planned changes in one mapped tree. Returns each op's result by id.
    pub fn apply_ops(
        &mut self,
        info: &Value,
        ops: &[PlannedOp],
    ) -> anyhow::Result<(BTreeMap<u64, OpResult>, Vec<String>)> {
        let mut results = BTreeMap::new();
        let mut warnings = Vec::new();
        let mut batch: Vec<Value> = Vec::new();
        let mut known = serde_json::Map::new();
        let mut size = 0;
        let mut send = |batch: &mut Vec<Value>,
                        known: &mut serde_json::Map<String, Value>,
                        this: &mut Self|
         -> anyhow::Result<()> {
            if batch.is_empty() {
                return Ok(());
            }
            let input = json!({ "mapping": info, "known": known, "ops": batch });
            let answer = this.run_json(&snippet(TWOWAY_APPLY, &input))?;
            for result in answer["results"].as_array().into_iter().flatten() {
                if let Some(id) = result["id"].as_u64() {
                    results.insert(
                        id,
                        OpResult {
                            ok: result["ok"] == true,
                            kept: result["kept"] == true,
                            error: result["error"].as_str().map(str::to_string),
                        },
                    );
                }
            }
            for warning in answer["warnings"].as_array().into_iter().flatten() {
                warnings.extend(warning.as_str().map(str::to_string));
            }
            batch.clear();
            known.clear();
            Ok(())
        };
        for planned in ops {
            let mut op = serde_json::to_value(&planned.op)?;
            if let Some(blob) = &planned.blob {
                op["blob64"] = Value::String(BASE64.encode(blob));
            }
            let op_size = op.to_string().len();
            if size + op_size > CALL_BYTES && !batch.is_empty() {
                send(&mut batch, &mut known, self)?;
                size = 0;
            }
            if let Some(source) = &planned.studio_source {
                known.insert(planned.op.key.clone(), Value::String(source.clone()));
            }
            size += op_size;
            batch.push(op);
        }
        send(&mut batch, &mut known, self)?;
        Ok((results, warnings))
    }

    /// Pushes Rojo's trees: makes Studio's copies match the built tree.
    pub fn push_rojo(
        &mut self,
        tree: &RojoTree,
        layout: &RojoLayout,
        dry: bool,
    ) -> anyhow::Result<RojoPushReport> {
        let mut report = RojoPushReport::default();
        let mut items = Vec::new();
        for path in &layout.trees {
            match find(tree.inner(), path) {
                Some(id) => {
                    collect_items(tree, id, path.clone(), &mut items)?;
                    report.trees.push(path.join("/"));
                }
                None => report
                    .errors
                    .push(format!("{}: Rojo built nothing there", path.join("/"))),
            }
        }
        let properties = property_code(tree.inner(), layout, &mut report.errors);

        // Batches of items; the property code goes with the last one.
        let mut batches: Vec<Vec<Value>> = vec![Vec::new()];
        let mut size = 0;
        for item in items {
            let value = item.to_json();
            let item_size = value.to_string().len();
            if size + item_size > CALL_BYTES && !batches.last().unwrap().is_empty() {
                batches.push(Vec::new());
                size = 0;
            }
            size += item_size;
            batches.last_mut().unwrap().push(value);
        }
        let count = batches.len();
        for (index, batch) in batches.into_iter().enumerate() {
            let driver = if index + 1 == count {
                ROJO_PUSH.replacen("--[[PROPERTIES]]", &properties, 1)
            } else {
                ROJO_PUSH.to_string()
            };
            let answer =
                self.run_json(&snippet(&driver, &json!({ "dry": dry, "items": batch })))?;
            report.add(&answer);
        }
        Ok(report)
    }
}

/// What Studio has under one mapping.
pub struct StudioRead {
    pub side: StudioSide,
    pub warnings: Vec<String>,
    /// Whether the mapping's root exists in Studio.
    pub found: bool,
}

impl StudioRead {
    fn parse(payload: &[u8]) -> anyhow::Result<Self> {
        let value: Value = serde_json::from_slice(payload).context("Studio's tree isn't JSON")?;
        let entries: Vec<StudioEntry> = serde_json::from_value(value["entries"].clone())
            .context("Studio's tree has an entry this version doesn't read")?;
        let mut models = BTreeMap::new();
        for model in value["models"].as_array().into_iter().flatten() {
            let key = model["key"].as_str().unwrap_or("").to_string();
            let data = BASE64.decode(model["data"].as_str().unwrap_or("").as_bytes())?;
            models.insert(key, data);
        }
        let warnings = value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| w.as_str().map(str::to_string))
            .collect();
        Ok(StudioRead {
            side: StudioSide { entries, models },
            warnings,
            found: value["found"] == true,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OpResult {
    pub ok: bool,
    pub kept: bool,
    pub error: Option<String>,
}

/// The description of a mapping the Studio module takes, as the server sends it on connect.
pub fn mapping_info(index: usize, mapping: &Mapping) -> anyhow::Result<Value> {
    let rules = ModelRule::for_mapping(mapping)?;
    Ok(json!({
        "index": index,
        "studio": mapping.studio,
        "models": mapping.models,
        "auto": rules.auto,
        "exclude": mapping.exclude,
        "group": rules.group,
        "separate": mapping.separate,
    }))
}

#[derive(Debug, Default, Serialize)]
pub struct RojoPushReport {
    pub trees: Vec<String>,
    /// The first paths of each kind of change, by kind.
    pub paths: BTreeMap<String, Vec<String>>,
    pub created: u64,
    pub updated: u64,
    pub replaced: u64,
    pub removed: u64,
    pub unchanged: u64,
    pub errors: Vec<String>,
}

impl RojoPushReport {
    fn add(&mut self, answer: &Value) {
        self.created += answer["created"].as_u64().unwrap_or(0);
        self.updated += answer["updated"].as_u64().unwrap_or(0);
        self.replaced += answer["replaced"].as_u64().unwrap_or(0);
        self.removed += answer["removed"].as_u64().unwrap_or(0);
        self.unchanged += answer["unchanged"].as_u64().unwrap_or(0);
        for error in answer["errors"].as_array().into_iter().flatten() {
            self.errors.extend(error.as_str().map(str::to_string));
        }
        for (kind, list) in answer["paths"].as_object().into_iter().flatten() {
            let paths = self.paths.entry(kind.clone()).or_default();
            for path in list.as_array().into_iter().flatten() {
                if paths.len() < 50 {
                    paths.extend(path.as_str().map(str::to_string));
                }
            }
        }
    }
}

struct RojoItem {
    path: Vec<String>,
    data: Vec<u8>,
    shell: bool,
    children: Vec<String>,
    keep_unknown: Vec<String>,
}

impl RojoItem {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "data64": BASE64.encode(&self.data),
            "shell": self.shell,
            "children": self.children,
            "keepUnknown": self.keep_unknown,
        })
    }
}

fn find(dom: &WeakDom, path: &[String]) -> Option<Ref> {
    let mut current = dom.root_ref();
    for name in path {
        current = dom
            .get_by_ref(current)?
            .children()
            .iter()
            .copied()
            .find(|child| dom.get_by_ref(*child).is_some_and(|i| &i.name == name))?;
    }
    Some(current)
}

/// Paths, relative to `root`, of the instances that keep what Rojo doesn't know about.
fn keep_unknown(tree: &RojoTree, root: Ref) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root, String::new())];
    while let Some((id, relative)) = stack.pop() {
        if tree
            .get_metadata(id)
            .is_some_and(|m| m.ignore_unknown_instances)
        {
            out.push(relative.clone());
        }
        if let Some(instance) = tree.inner().get_by_ref(id) {
            for child in instance.children() {
                let Some(child_instance) = tree.inner().get_by_ref(*child) else {
                    continue;
                };
                let child_relative = if relative.is_empty() {
                    child_instance.name.clone()
                } else {
                    format!("{relative}/{}", child_instance.name)
                };
                stack.push((*child, child_relative));
            }
        }
    }
    out
}

/// Items for one tree: whole when it fits in a call, else a shell and its children as items.
fn collect_items(
    tree: &RojoTree,
    id: Ref,
    path: Vec<String>,
    items: &mut Vec<RojoItem>,
) -> anyhow::Result<()> {
    let dom = tree.inner();
    let instance = dom
        .get_by_ref(id)
        .ok_or_else(|| anyhow!("{} is gone from the tree", path.join("/")))?;
    let mut whole = Vec::new();
    rbx_binary::to_writer(&mut whole, dom, &[id])?;

    let children: Vec<Ref> = instance.children().to_vec();
    let names: Vec<String> = children
        .iter()
        .filter_map(|c| dom.get_by_ref(*c).map(|i| i.name.clone()))
        .collect();
    let unique = names.iter().collect::<BTreeSet<_>>().len() == names.len();
    if whole.len() * 4 / 3 <= CALL_BYTES || children.is_empty() || !unique {
        items.push(RojoItem {
            path,
            data: whole,
            shell: false,
            children: Vec::new(),
            keep_unknown: keep_unknown(tree, id),
        });
        return Ok(());
    }

    // The instance alone, then each child.
    let alone = WeakDom::new(
        InstanceBuilder::new(instance.class)
            .with_name(instance.name.as_str())
            .with_properties(instance.properties.clone()),
    );
    let mut shell = Vec::new();
    rbx_binary::to_writer(&mut shell, &alone, &[alone.root_ref()])?;
    let keeps = tree
        .get_metadata(id)
        .is_some_and(|m| m.ignore_unknown_instances);
    items.push(RojoItem {
        path: path.clone(),
        data: shell,
        shell: true,
        children: names,
        keep_unknown: if keeps {
            vec![String::new()]
        } else {
            Vec::new()
        },
    });
    for child in children {
        let name = dom
            .get_by_ref(child)
            .map(|i| i.name.clone())
            .unwrap_or_default();
        let mut child_path = path.clone();
        child_path.push(name);
        collect_items(tree, child, child_path, items)?;
    }
    Ok(())
}

/// Luau that sets the properties and attributes project nodes give instances that are already
/// there, such as `HttpService.HttpEnabled`. Values Luau can't be written for are reported.
fn property_code(dom: &WeakDom, layout: &RojoLayout, errors: &mut Vec<String>) -> String {
    let database = rbx_reflection_database::get().ok();
    let mut code = String::new();
    for node in &layout.properties {
        let path = &node.path;
        let Some(id) = find(dom, path) else { continue };
        let Some(instance) = dom.get_by_ref(id) else {
            continue;
        };
        let label = path.join("/");
        let path_literal = format!(
            "{{{}}}",
            path.iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut lines = Vec::new();
        for (name, value) in &instance.properties {
            let name = name.as_str();
            if !node.names.iter().any(|n| n == name) {
                continue;
            }
            if name == "Attributes" {
                if let Variant::Attributes(attributes) = value {
                    for (attribute, value) in attributes.iter() {
                        match luau_value(value, None) {
                            Some(literal) => lines.push(format!(
                                "changed = SetAttribute(target, {attribute:?}, {literal}) or changed"
                            )),
                            None => errors.push(format!(
                                "{label}: attribute {attribute} has a type a push can't set"
                            )),
                        }
                    }
                }
                continue;
            }
            if matches!(name, "Tags" | "Name") {
                continue;
            }
            let enum_name = database.and_then(|db| {
                let class = db.classes.get(instance.class.as_str())?;
                db.superclasses_iter(class)
                    .find_map(|c| match &c.properties.get(name)?.data_type {
                        rbx_reflection::DataType::Enum(e) => Some(e.to_string()),
                        _ => None,
                    })
            });
            match luau_value(value, enum_name.as_deref()) {
                Some(literal) => lines.push(format!(
                    "changed = SetProperty(target, {name:?}, {literal}) or changed"
                )),
                None => errors.push(format!("{label}.{name} has a type a push can't set")),
            }
        }
        if lines.is_empty() {
            continue;
        }
        // The helpers compare first and respect a dry run (rojo_push.luau).
        code.push_str(&format!(
            "do\n\tlocal target = Resolve({path_literal}, false)\n\tlocal changed = false\n\tlocal ok, problem = pcall(function()\n\t\tif not target then error(\"it isn't in the place\") end\n{}\n\tend)\n\tif not ok then table.insert(stats.errors, {label:?} .. \": \" .. tostring(problem)) elseif changed then Count(\"updated\", {label:?}) else stats.unchanged += 1 end\nend\n",
            lines.iter().map(|l| format!("\t\t{l}")).collect::<Vec<_>>().join("\n")
        ));
    }
    code
}

/// A Luau expression for a value, for the types projects set on existing instances.
fn luau_value(value: &Variant, enum_name: Option<&str>) -> Option<String> {
    let number = |n: f64| {
        if n.is_finite() {
            format!("{n}")
        } else if n.is_nan() {
            "0/0".to_string()
        } else if n > 0.0 {
            "math.huge".to_string()
        } else {
            "-math.huge".to_string()
        }
    };
    Some(match value {
        Variant::Bool(b) => b.to_string(),
        Variant::Int32(n) => n.to_string(),
        Variant::Int64(n) => n.to_string(),
        Variant::Float32(n) => number(f64::from(*n)),
        Variant::Float64(n) => number(*n),
        Variant::String(s) => format!("{:?}", s),
        Variant::Color3(c) => format!(
            "Color3.new({}, {}, {})",
            number(c.r.into()),
            number(c.g.into()),
            number(c.b.into())
        ),
        Variant::Color3uint8(c) => format!("Color3.fromRGB({}, {}, {})", c.r, c.g, c.b),
        Variant::Vector2(v) => format!(
            "Vector2.new({}, {})",
            number(v.x.into()),
            number(v.y.into())
        ),
        Variant::Vector3(v) => format!(
            "Vector3.new({}, {}, {})",
            number(v.x.into()),
            number(v.y.into()),
            number(v.z.into())
        ),
        Variant::UDim(u) => format!("UDim.new({}, {})", number(u.scale.into()), u.offset),
        Variant::UDim2(u) => format!(
            "UDim2.new({}, {}, {}, {})",
            number(u.x.scale.into()),
            u.x.offset,
            number(u.y.scale.into()),
            u.y.offset
        ),
        Variant::NumberRange(r) => format!(
            "NumberRange.new({}, {})",
            number(r.min.into()),
            number(r.max.into())
        ),
        Variant::Rect(r) => format!(
            "Rect.new({}, {}, {}, {})",
            number(r.min.x.into()),
            number(r.min.y.into()),
            number(r.max.x.into()),
            number(r.max.y.into())
        ),
        Variant::BrickColor(b) => format!("BrickColor.new({})", *b as u16),
        Variant::Enum(e) => {
            let name = enum_name?;
            format!(
                "(function() for _, item in Enum.{name}:GetEnumItems() do if item.Value == {} then return item end end end)()",
                e.to_u32()
            )
        }
        _ => return None,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn long_strings_survive_any_text() {
        assert_eq!(long_string("abc"), "[[\nabc]]");
        assert_eq!(long_string("a]]b"), "[=[\na]]b]=]");
        assert_eq!(long_string("a]]b]=]c"), "[==[\na]]b]=]c]==]");
    }

    #[test]
    fn the_checksum_matches_fnv1a() {
        // Known FNV-1a 32 values.
        assert_eq!(checksum(b""), 0x811c9dc5);
        assert_eq!(checksum(b"a"), 0xe40c292c);
        assert_eq!(checksum(b"foobar"), 0xbf9cf968);
    }

    #[test]
    fn answers_are_read_between_markers() {
        assert_eq!(framed("\"<<abc>>\"").unwrap(), "abc");
        assert!(framed("nothing").is_err());
    }

    #[test]
    fn studio_errors_lose_the_assistants_locations() {
        assert_eq!(
            clean_error("sabuiltin_Assistant.ExecuteLuauTool:66: AssistantCommand:5: boom"),
            "boom"
        );
        assert_eq!(clean_error("plain: message"), "plain: message");
    }

    fn window(id: &str, name: Option<&str>) -> StudioWindow {
        StudioWindow {
            id: id.into(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn a_window_is_picked_by_id_name_place_or_part() {
        let windows = [
            window("a", Some("Dive In (placeId: 111)")),
            window("b", Some("MainPlace.rbxl")),
            window("c", None),
        ];
        assert_eq!(pick_window(&windows, Some("a")).unwrap().id, "a");
        assert_eq!(pick_window(&windows, Some("Dive In")).unwrap().id, "a");
        assert_eq!(pick_window(&windows, Some("111")).unwrap().id, "a");
        assert_eq!(pick_window(&windows, Some("mainplace")).unwrap().id, "b");
        assert!(pick_window(&windows, None).is_err(), "two named windows");
        assert!(pick_window(&windows, Some("nope")).is_err());
        assert_eq!(pick_window(&windows[1..], None).unwrap().id, "b");
    }

    #[test]
    fn the_modules_inline_with_their_require_replaced() {
        let prelude = modules();
        assert!(!prelude.contains("require(script.Parent"));
        assert!(prelude.contains("local TwoWayStudio = (function()"));
    }

    #[test]
    fn values_become_luau() {
        assert_eq!(luau_value(&Variant::Bool(true), None).unwrap(), "true");
        assert_eq!(
            luau_value(&Variant::String("a\"b".into()), None).unwrap(),
            "\"a\\\"b\""
        );
        assert!(luau_value(&Variant::Enum(rbx_dom_weak::types::Enum::from_u32(1)), None).is_none());
    }
}

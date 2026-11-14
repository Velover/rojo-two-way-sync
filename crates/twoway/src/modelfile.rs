//! Model files: binary `.rbxm` or XML `.rbxmx`.
//!
//! Studio only reads and writes binary, so XML is converted on the way in and out. Both encoders
//! are deterministic (rbx_xml numbers referents in order and sorts properties), so the same
//! instances always give the same bytes, and the hashes of both sides can be compared.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use rbx_dom_weak::types::Ref;
use rbx_dom_weak::WeakDom;
use serde::{Deserialize, Serialize};

use crate::tree::hash_bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFormat {
    Rbxm,
    #[default]
    Rbxmx,
}

impl ModelFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ModelFormat::Rbxm => "rbxm",
            ModelFormat::Rbxmx => "rbxmx",
        }
    }

    /// What the bytes are, whatever the file is called.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"<roblox!") {
            ModelFormat::Rbxm
        } else {
            ModelFormat::Rbxmx
        }
    }
}

/// Splits `Name.rbxm` or `Name.rbxmx` into the name and format.
pub fn parse_model_file(file_name: &str) -> Option<(&str, ModelFormat)> {
    if let Some(name) = file_name.strip_suffix(".rbxmx") {
        Some((name, ModelFormat::Rbxmx))
    } else {
        file_name
            .strip_suffix(".rbxm")
            .map(|name| (name, ModelFormat::Rbxm))
    }
}

pub fn decode(bytes: &[u8]) -> Result<WeakDom> {
    match ModelFormat::of_bytes(bytes) {
        ModelFormat::Rbxm => rbx_binary::from_reader(bytes).context("could not read the .rbxm"),
        ModelFormat::Rbxmx => {
            // Properties newer than the bundled reflection database are kept, not dropped.
            let options = rbx_xml::DecodeOptions::new()
                .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown);
            rbx_xml::from_reader(bytes, options)
                .map_err(|e| anyhow!("could not read the .rbxmx: {e}"))
        }
    }
}

pub fn encode(dom: &WeakDom, roots: &[Ref], format: ModelFormat) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match format {
        ModelFormat::Rbxm => rbx_binary::to_writer(&mut out, dom, roots)?,
        ModelFormat::Rbxmx => {
            let options = rbx_xml::EncodeOptions::new()
                .property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown);
            rbx_xml::to_writer(&mut out, dom, roots, options)
                .map_err(|e| anyhow!("could not write XML: {e}"))?;
        }
    }
    Ok(out)
}

/// The same instances in the other format. Bytes already in that format are returned as they are.
pub fn convert(bytes: &[u8], to: ModelFormat) -> Result<Vec<u8>> {
    if ModelFormat::of_bytes(bytes) == to {
        return Ok(bytes.to_vec());
    }
    let dom = decode(bytes)?;
    encode(&dom, dom.root().children(), to)
}

/// The hash a model file is compared by: of its bytes when it's in the mapping's format, or of
/// what it converts to. XML is also checked for being readable. Results are remembered, since
/// the same files are scanned again and again.
pub fn model_hash(bytes: &[u8], format: ModelFormat) -> Result<String> {
    type Seen = HashMap<(String, ModelFormat), Result<String, String>>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let raw = hash_bytes(bytes);
    let cache = SEEN.get_or_init(Default::default);
    if let Some(result) = cache.lock().unwrap().get(&(raw.clone(), format)) {
        return result.clone().map_err(|e| anyhow!(e));
    }
    let result = if ModelFormat::of_bytes(bytes) == format {
        match format {
            ModelFormat::Rbxm => Ok(raw.clone()),
            ModelFormat::Rbxmx => decode(bytes).map(|_| raw.clone()),
        }
    } else {
        convert(bytes, format).map(|converted| hash_bytes(&converted))
    };
    let result = result.map_err(|e| format!("{e:#}"));
    cache.lock().unwrap().insert((raw, format), result.clone());
    result.map_err(|e| anyhow!(e))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use rbx_dom_weak::types::{Attributes, Variant};
    use rbx_dom_weak::{ustr, InstanceBuilder};

    /// A small model, as binary.
    pub fn sample() -> Vec<u8> {
        sample_with(0.25)
    }

    /// Two loose instances, as a folder's shared file holds them.
    pub fn sample_group(density: f32) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let sky = dom.insert(root, InstanceBuilder::new("Sky").with_name("Sunless sky"));
        let air = dom.insert(
            root,
            InstanceBuilder::new("Atmosphere")
                .with_name("Atmosphere")
                .with_property("Density", density),
        );
        encode(&dom, &[sky, air], ModelFormat::Rbxm).unwrap()
    }

    /// The same model with another Volume.
    pub fn sample_with(volume: f32) -> Vec<u8> {
        let mut attributes = Attributes::new();
        attributes.insert("Volume".into(), Variant::Float64(0.5));
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let folder = dom.insert(root, InstanceBuilder::new("Folder").with_name("Sounds"));
        dom.insert(
            folder,
            InstanceBuilder::new("Sound")
                .with_name("Wind")
                .with_property("Volume", volume)
                .with_property("Attributes", attributes),
        );
        dom.insert(
            folder,
            InstanceBuilder::new("Atmosphere")
                .with_name("Air")
                .with_property("Density", 0.3f32),
        );
        encode(&dom, &[folder], ModelFormat::Rbxm).unwrap()
    }

    #[test]
    fn formats_are_told_apart_by_content() {
        assert_eq!(ModelFormat::of_bytes(&sample()), ModelFormat::Rbxm);
        assert_eq!(
            ModelFormat::of_bytes(&convert(&sample(), ModelFormat::Rbxmx).unwrap()),
            ModelFormat::Rbxmx
        );
        assert_eq!(parse_model_file("A.rbxmx"), Some(("A", ModelFormat::Rbxmx)));
        assert_eq!(parse_model_file("A.rbxm"), Some(("A", ModelFormat::Rbxm)));
        assert_eq!(parse_model_file("A.luau"), None);
    }

    #[test]
    fn xml_is_the_same_every_time() {
        let a = convert(&sample(), ModelFormat::Rbxmx).unwrap();
        let b = convert(&sample(), ModelFormat::Rbxmx).unwrap();
        assert_eq!(a, b);
        let text = String::from_utf8(a.clone()).unwrap();
        assert!(text.contains("<Item class=\"Sound\""), "{text}");
        assert!(text.contains('\n'), "indented, so it diffs well");
        // Through binary and back gives the same XML.
        let again = convert(&convert(&a, ModelFormat::Rbxm).unwrap(), ModelFormat::Rbxmx).unwrap();
        assert_eq!(a, again);
    }

    #[test]
    fn a_round_trip_keeps_the_instances() {
        let xml = convert(&sample(), ModelFormat::Rbxmx).unwrap();
        let dom = decode(&xml).unwrap();
        let folder = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(folder.name, "Sounds");
        let names: Vec<_> = folder
            .children()
            .iter()
            .map(|c| dom.get_by_ref(*c).unwrap().name.clone())
            .collect();
        assert_eq!(names, ["Wind", "Air"]);
        let wind = dom.get_by_ref(folder.children()[0]).unwrap();
        assert!(matches!(
            wind.properties.get(&ustr("Attributes")),
            Some(Variant::Attributes(_))
        ));
    }

    /// Real files saved by Studio, listed in `TWOWAY_SAMPLE_MODELS` (separated by `;`): XML is
    /// stable and keeps every instance and property.
    #[test]
    #[ignore]
    fn real_studio_models_survive_xml() {
        let list = std::env::var("TWOWAY_SAMPLE_MODELS").expect("set TWOWAY_SAMPLE_MODELS");
        for path in list.split(';').filter(|p| !p.is_empty()) {
            let binary = std::fs::read(path).unwrap();
            let xml =
                convert(&binary, ModelFormat::Rbxmx).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert_eq!(
                xml,
                convert(&binary, ModelFormat::Rbxmx).unwrap(),
                "{path}: not deterministic"
            );
            let back = convert(&xml, ModelFormat::Rbxm).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert_eq!(
                xml,
                convert(&back, ModelFormat::Rbxmx).unwrap(),
                "{path}: changes through binary"
            );
            let differences = crate::modeldiff::diff(&binary, &xml).unwrap();
            assert!(differences.is_empty(), "{path}: {differences:#?}");
            println!(
                "{path}: {} bytes binary, {} bytes XML",
                binary.len(),
                xml.len()
            );
        }
    }

    #[test]
    fn hashes_compare_across_formats() {
        let binary = sample();
        let xml = convert(&binary, ModelFormat::Rbxmx).unwrap();
        assert_eq!(
            model_hash(&binary, ModelFormat::Rbxmx).unwrap(),
            hash_bytes(&xml)
        );
        assert_eq!(
            model_hash(&xml, ModelFormat::Rbxmx).unwrap(),
            hash_bytes(&xml)
        );
        assert_eq!(
            model_hash(&binary, ModelFormat::Rbxm).unwrap(),
            hash_bytes(&binary)
        );
        assert!(model_hash(b"<roblox><Item class=", ModelFormat::Rbxmx).is_err());
    }
}

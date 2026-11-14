//! The fork's two-way trees in `rojo build`: a project with a `twoway.json` beside it builds a
//! place with the mapped folders' files in it.

use std::{path::Path, process::Command};

use rbx_dom_weak::WeakDom;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, BUILD_TESTS_PATH, ROJO_PATH};

fn build(output_name: &str) -> WeakDom {
    let input_path = Path::new(BUILD_TESTS_PATH).join("twoway_trees");
    let output_dir = tempdir().expect("couldn't create temporary directory");
    let output_path = output_dir.path().join(output_name);

    let output = Command::new(ROJO_PATH)
        .args([
            "build",
            input_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(get_working_dir_path())
        .output()
        .expect("Couldn't start Rojo");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "Rojo did not exit successfully");

    let file = fs_err::File::open(&output_path).expect("Couldn't read output file");
    if output_name.ends_with(".rbxlx") || output_name.ends_with(".rbxmx") {
        rbx_xml::from_reader_default(file).unwrap()
    } else {
        rbx_binary::from_reader(file).unwrap()
    }
}

/// The class and `Source` of the instance at a path of names, if there is one.
fn find(dom: &WeakDom, path: &str) -> Option<(String, Option<String>)> {
    let mut current = dom.root_ref();
    for name in path.split('/') {
        current = dom
            .get_by_ref(current)?
            .children()
            .iter()
            .copied()
            .find(|child| dom.get_by_ref(*child).is_some_and(|i| i.name == name))?;
    }
    let instance = dom.get_by_ref(current)?;
    let source = match instance.properties.get(&rbx_dom_weak::ustr("Source")) {
        // A checkout may turn the test files' line endings into CRLF.
        Some(rbx_dom_weak::types::Variant::String(text)) => Some(text.replace("\r\n", "\n")),
        _ => None,
    };
    Some((instance.class.to_string(), source))
}

#[test]
fn a_place_build_has_the_two_way_trees() {
    let dom = build("Place.rbxlx");

    let (class, source) = find(&dom, "ReplicatedStorage/Rojo/Shared").expect("Rojo's tree");
    assert_eq!(class, "ModuleScript");
    assert_eq!(source.as_deref(), Some("return \"from rojo\"\n"));

    let (class, source) =
        find(&dom, "ReplicatedStorage/GameData/Config").expect("the two-way tree");
    assert_eq!(class, "ModuleScript");
    assert_eq!(
        source.as_deref(),
        Some("return \"from the two-way tree\"\n")
    );
    assert_eq!(
        find(&dom, "ReplicatedStorage/GameData/Settings").map(|(class, _)| class),
        Some("Folder".to_string())
    );
    assert!(find(&dom, "ReplicatedStorage/GameData/Settings/Audio").is_some());

    assert!(
        find(&dom, "Workspace/Elsewhere").is_none(),
        "a mapping for another place stays out"
    );
}

#[test]
fn a_binary_place_build_has_them_too() {
    let dom = build("Place.rbxl");
    assert!(find(&dom, "ReplicatedStorage/GameData/Config").is_some());
}

#[test]
fn a_mapping_for_a_place_goes_into_the_place_of_that_name() {
    let dom = build("Other.rbxlx");
    assert!(find(&dom, "Workspace/Elsewhere/Marker").is_some());
}

#[test]
fn building_leaves_no_sync_state_behind() {
    build("Place.rbxlx");
    assert!(
        !Path::new(BUILD_TESTS_PATH)
            .join("twoway_trees")
            .join(".twoway")
            .exists(),
        "only `rojo serve` keeps sync state"
    );
}

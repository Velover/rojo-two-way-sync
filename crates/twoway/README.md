# twoway

Two-way sync between Roblox Studio and disk, for the folders a `twoway.json` lists. It is part of
this Rojo fork: Rojo keeps pushing its trees one way, and twoway keeps the listed trees the same in
both directions, with one `rojo serve` and one Connect. Without a `twoway.json` beside the project,
the fork behaves like stock Rojo.

- Scripts and folders sync both ways, live, with their attributes and tags.
- Every other instance is saved too, as XML `.rbxmx` you can read and edit: a folder's instances
  share one `init.rbxmx`, and a subtree you list gets a file of its own.
- Editing a script from disk doesn't move the cursor in Studio.
- Anything else in Studio is left alone and never deleted.
- When both sides changed, a local page shows the diff and you choose, or an agent does with
  `rojo twoway`.

This crate is the engine. Rojo's `src/twoway_sync.rs` mounts it on the server, `src/cli/twoway.rs`
adds the commands, and `plugin/src/TwoWay.lua` is the Studio side.

## Setup

1. Install the fork's `rojo` and its plugin.
2. Turn off Studio's Script Sync for the folders you map here. Two tools writing the same files
   will fight.
3. Put a `twoway.json` beside the project file:

```json
{
	"mappings": [
		{
			"studio": "Workspace/Cutscenes",
			"path": "synced/Cutscenes",
			"placeId": 1234567890,
			"instances": "listed",
			"models": ["Intro/Assets", "Intro/Settings"]
		},
		{
			"studio": "ReplicatedStorage/Data",
			"path": "synced/Data",
			"models": ["Biomes/*/Rig"],
			"separate": ["Biomes/*/Sounds"],
			"exclude": ["Biomes/*/Map"]
		}
	],
	"ignore": ["**/*.md"]
}
```

| Field | Meaning |
|---|---|
| `mappings[].studio` | The instance path, from the DataModel. It may not overlap a tree the project fills from files (`$path`), nor sit inside a node with `$ignoreUnknownInstances: false`. |
| `mappings[].path` | The folder, relative to `twoway.json`. |
| `mappings[].placeId`, `placeName` | Which place this mapping belongs to. `placeName` is `game.Name`, the file name for a local place. A mapping with neither syncs in any place Rojo connects to, and goes into every place `rojo build` makes. |
| `mappings[].instances` | `auto` (the default) saves every instance under `studio`. `listed` (or `manual`, or `none`) saves only scripts, Folders and what `models` lists. |
| `mappings[].models` | Instance paths under `studio`, each saved whole as one model file, scripts and Folders inside it included. Use it to keep a folder as one file. `*` stands for any one name, so `Biomes/*/Rig` packs every biome's Rig; `**` stands for any depth. |
| `mappings[].groupInstances` | Default `true`: a folder's instances that aren't scripts or Folders share one file, `init.rbxmx` in its directory. `false` gives each its own file instead, `Name.rbxmx`. |
| `mappings[].separate` | Folders whose instances get a file each even when grouping is on. Patterns as in `models`. |
| `mappings[].exclude` | Instance paths under `studio` that never sync. Patterns as in `models`. |
| `mappings[].modelFormat` | `rbxmx` (the default, XML) or `rbxm` (binary). A file already in the other format is converted the next time it syncs. |
| `ignore` | Globs for files to leave out, relative to the mapping's folder. |

`port` and `autoConnect`, from when twoway ran on its own, are accepted and ignored.

## Syncing

Start Rojo as usual, with `rojo serve` or the VS Code extension. It prints "Two-way sync is on for
N tree(s)" and serves the sync under `/twoway/` on Rojo's port. Rojo's **Connect** in Studio starts
both syncs, and the plugin's connected page shows a **Two-way sync** box: how many trees sync both
ways, how many items wait for review with a button for the page, and recent warnings.
**Disconnect** stops both.

The review page is at `http://localhost:34872/twoway/` (Rojo's port). It opens by itself when
something needs a decision, unless `ROJO_TWOWAY_NO_OPEN` is set, and shows recent changes otherwise.
On the page, **Diff in VS Code** and **Merge in VS Code** open the conflict in VS Code's diff and
3-way merge editors.

## From the command line

`rojo twoway` does what the review page does, for people and agents. It prints JSON, or text with
`--text`. `--project` and `--port` pick the project and the server when they aren't the current
folder and the project's port.

```sh
rojo twoway status                        # running? connected place, conflicts, warnings
rojo twoway conflicts                     # what waits, each with a token
rojo twoway show 3                        # both sides, the last agreed version, a diff
rojo twoway resolve 3 --keep disk --token <token from show>
rojo twoway resolve 3 --merged merged.luau --token <token>
rojo twoway history --path GameData/Diving --limit 10
rojo twoway restore 8092293e32 StudioSynced/GameData/Diving.luau
```

- Conflicts only exist while `rojo serve` runs, so `status`, `conflicts`, `show` and `resolve` ask
  the server. `resolve` with `--token` is refused if either side changed since the token was
  printed.
- `history` and `restore` read `.twoway/` and work without a server. `restore` writes a stored
  version back to a file in a mapped folder; a running sync sends it to Studio like any edit.

## Building places

`rojo build -o Place.rbxl` puts the mapped trees into the place it builds from the project, so one
build makes a complete place. Mappings that name no place go in, and those that name this place by
the project's `placeId` or the output's file name. Model builds (`.rbxm`, `.rbxmx`) stay as they
were. `--watch` rebuilds when Rojo's files change, not the mapped folders'.

```sh
rojo twoway build --base MainPlace.rbxl -o Build.rbxl    # keep the saved place's map and the rest
rojo twoway check --place MainPlace.rbxl                 # what a first sync would do; writes nothing
```

`rojo twoway build` starts from a saved place instead: the project's trees replace the place's,
then the mapped files go in, and everything else in the place stays. It uses mappings that name no
place, name the base's file, or match `--place-id` (by default the project's `placeId`). The base
is only read.

`check` compares the folders with a saved place and lists what a first sync would add, write and
ask about. The place may be older than what's open in Studio, so treat it as a preview.

## What `.twoway/` holds

`.twoway/` beside `twoway.json` holds the last agreed version of everything, every version seen
(by hash, in `blobs/`), and the history. It ignores itself in git. Never edit or delete it while
Rojo runs.

It doesn't grow without end: when `rojo serve` starts, it removes the diff and merge files of
earlier runs, history past 5000 entries, and stored versions that neither the agreed state (with up
to 100 past versions per item) nor the kept history refers to. A conflict's diff and merge files go
as soon as it's settled. `rojo build` and the `rojo twoway` commands that only look write nothing
there.

## File names

New files follow Rojo: `Name.luau` (ModuleScript), `Name.server.luau` (Script), `Name.client.luau`
(LocalScript), `Name.plugin.luau`, and `init.*` for a script with children.

Script Sync's `.local.luau` and `.legacy.luau` still work and are never renamed. They only become
Rojo names if you rename them, or re-sync them as new files.

## Attributes and tags

They live in Rojo's meta files, so Rojo reads them the same way: `Name.meta.json` beside a script,
and `init.meta.json` inside a folder.

```json
{
	"attributes": { "Weight": 12.5, "Tint": { "Color3": [0.1, 0.2, 0.3] } },
	"properties": { "Tags": ["Spawn"] }
}
```

- Every attribute type Studio has is supported: string, boolean, number, Vector2, Vector3, Color3,
  UDim, UDim2, NumberRange, Rect, BrickColor, CFrame, NumberSequence, ColorSequence, Font and enums.
- Anything else in a meta file, such as `ignoreUnknownInstances`, is kept. A meta file that isn't
  valid JSON is never overwritten: it shows up as a warning on the review page instead.
- Studio stores most typed values at 32-bit precision, so `0.1` in a file and Studio's
  `0.10000000149` count as the same value and nothing gets rewritten.
- Folders and scripts made by Script Sync have no meta files. On a first sync where the text
  matches, Studio's attributes are simply written; that isn't a conflict.
- `rojo build` and `rojo twoway build` set attributes and tags from meta files. Where there is no meta file, the place keeps
  its own.

## Model files

With the defaults, a folder like this in Studio:

```
Base (Folder)
├── Atmosphere
├── Sunless sky
├── Bloom
├── Lighting (ModuleScript)
└── Sounds (Folder)
    └── Wind (Sound)
```

is saved as:

```
Base/
├── init.rbxmx        Atmosphere, Sunless sky and Bloom, with everything inside them
├── Lighting.luau
└── Sounds/
    └── init.rbxmx    Wind
```

A change to one instance rewrites its folder's `init.rbxmx`, and a conflict on it is settled for the
whole file. With `"separate": ["Base/Sounds"]`, `Wind` would be `Sounds/Wind.rbxmx` instead; with
`"models": ["Base/Sounds"]`, the Sounds folder would be one `Base/Sounds.rbxmx`. Two instances in a
shared file may have the same name. An instance called `init` isn't synced.

Studio only reads and writes binary, so with `rbxmx` twoway converts on the way: Studio's version is written as XML, and an XML file you
edit reaches Studio as binary. Both encoders give the same bytes for the same instances, so nothing
is rewritten when nothing changed. A file you edited by hand is rewritten the next time the
instance changes in Studio, in twoway's own layout.

A model file that can't be read, such as XML broken by an edit, is left alone on both sides with a
warning until it's fixed: it isn't treated as deleted.

In auto mode, instances with `Archivable` off, and any Studio can't save, are left out. A model
file that doesn't fit the folder's setting, such as `Sky.rbxmx` in a folder that groups its
instances, isn't synced, and a warning says so.

## Which folders sync

A Folder syncs when it has something to sync: a script or model somewhere inside, or attributes or
tags of its own. In `listed` mode, a Folder holding only other instances, such as a folder of Sounds,
doesn't become an empty directory; list it in `models` if you want it on disk. A directory holding
only notes doesn't become a Folder.

## What happens when

| Situation | Result |
|---|---|
| Only one side has it, on a first sync | Copied to the other side |
| One side changed | Copied to the other side |
| Both sides are the same | Nothing |
| Both changed | Review. Only that item stops syncing |
| Both have it on a first sync, and they differ | Review |
| Deleted while nothing was connected | Review, so nothing is deleted by surprise |
| Studio shows up with an older version on connect | Review, with **Keep disk** suggested: usually a place closed without saving |
| A folder is deleted on disk, but holds things that aren't synced in Studio | Studio keeps it |
| The last script in a folder is deleted | The emptied Folder, or directory, goes too |

## Limits

- Models are checked every 5 seconds. A very large model costs a short pause each time, and in
  auto mode every instance under the root counts: exclude big trees such as maps.
- A reference from outside a model into it is lost when the model is replaced from disk in Studio.
  `rojo twoway build` repairs those by path where it can, and prints the rest.
- Two siblings with the same name can't both be files, so they're skipped with a warning.
- Team Create has no lock yet: only one person should run the sync on a mapping.

## Tests

`cargo test -p twoway` runs the engine, naming, meta file, build, model diff and cleanup tests
(`-- --ignored` also opens a real browser tab). Rojo's `cargo test` covers the commands and
`rojo build` with a `twoway.json` (`tests/tests/twoway.rs`).

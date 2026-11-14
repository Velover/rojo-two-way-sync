//! A small client for Roblox Studio's MCP server, which lets a program list the open Studio windows
//! and run Luau in one of them (`execute_luau`).
//!
//! The server is `StudioMCP.exe`, which ships with Studio, spoken to as JSON-RPC over stdio. The
//! first one started becomes a hub that the open Studios join, and later ones join the hub, so this
//! works next to an assistant's own MCP connection.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};

/// Overrides where the MCP server is.
pub const MCP_EXE_VAR: &str = "ROJO_STUDIO_MCP_EXE";

const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the server gets to leave the hub once its stdin closes, before it is ended.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// One open Studio window, as the server lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct StudioWindow {
    pub id: String,
    /// "Place (placeId: 123)" for a published place, the file name for a local one, and none for
    /// a window still loading or a test session's.
    pub name: Option<String>,
}

impl StudioWindow {
    pub fn place_id(&self) -> Option<u64> {
        let name = self.name.as_deref()?;
        let start = name.rfind("(placeId: ")? + "(placeId: ".len();
        name[start..].trim_end_matches(')').trim().parse().ok()
    }

    /// The name Studio shows for the place, without the place id.
    pub fn place_name(&self) -> Option<&str> {
        let name = self.name.as_deref()?;
        Some(match name.rfind(" (placeId: ") {
            Some(index) => &name[..index],
            None => name,
        })
    }

    /// The file name of a window with a local place file open.
    pub fn file_name(&self) -> Option<&str> {
        let name = self.name.as_deref()?;
        let lower = name.to_ascii_lowercase();
        (self.place_id().is_none() && (lower.ends_with(".rbxl") || lower.ends_with(".rbxlx")))
            .then_some(name)
    }
}

/// The newest `StudioMCP.exe` under Studio's versions folder, or the override.
fn find_server() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os(MCP_EXE_VAR) {
        return Ok(PathBuf::from(path));
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| {
            anyhow!("Studio's MCP server is only found on Windows; set {MCP_EXE_VAR}")
        })?;
    let versions = local.join("Roblox").join("Versions");
    let newest = fs_err::read_dir(&versions)
        .with_context(|| {
            format!(
                "Roblox Studio doesn't seem to be installed ({})",
                versions.display()
            )
        })?
        .flatten()
        .map(|entry| entry.path().join("StudioMCP.exe"))
        .filter(|path| path.is_file())
        .max_by_key(|path| path.metadata().and_then(|m| m.modified()).ok());
    newest.ok_or_else(|| {
        anyhow!(
            "no StudioMCP.exe under {}: update Studio, or set {MCP_EXE_VAR}",
            versions.display()
        )
    })
}

pub struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
    /// Answers that arrived while waiting for another.
    stash: VecDeque<Value>,
    next_id: u64,
}

impl McpClient {
    /// Starts the server and does the MCP handshake.
    pub fn connect() -> anyhow::Result<Self> {
        let exe = find_server()?;
        let mut command = Command::new(&exe);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window of its own.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("couldn't start {}", exe.display()))?;

        let stdout = child.stdout.take().expect("stdout is piped");
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // The server also logs plain text on stdout.
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if sender.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || for _ in BufReader::new(stderr).lines() {});
        }

        let stdin = child.stdin.take();
        let mut client = McpClient {
            child,
            stdin,
            messages,
            stash: VecDeque::new(),
            next_id: 1,
        };
        client.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "rojo", "version": env!("CARGO_PKG_VERSION") },
            }),
            SETUP_TIMEOUT,
        )?;
        client.send(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        )?;
        Ok(client)
    }

    fn send(&mut self, message: &Value) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the MCP server is closed"))?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let deadline = Instant::now() + timeout;
        loop {
            if let Some(index) = self.stash.iter().position(|m| m["id"] == id) {
                return Self::result_of(method, self.stash.remove(index).unwrap());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(left) {
                Ok(message) if message["id"] == id => return Self::result_of(method, message),
                Ok(message) => self.stash.push_back(message),
                Err(RecvTimeoutError::Timeout) => {
                    bail!(
                        "Studio's MCP server didn't answer {method} within {}s",
                        timeout.as_secs()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => bail!("Studio's MCP server exited"),
            }
        }
    }

    fn result_of(method: &str, message: Value) -> anyhow::Result<Value> {
        if let Some(error) = message.get("error") {
            let text = error["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            bail!("{method}: {text}");
        }
        Ok(message["result"].clone())
    }

    /// Calls a tool. Returns its text, and whether it reported an error.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> anyhow::Result<(String, bool)> {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
            timeout,
        )?;
        let text = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok((text, result["isError"] == true))
    }

    /// The open Studio windows. The server joins the hub a moment after it starts, so this retries
    /// for a few seconds.
    pub fn studios(&mut self) -> anyhow::Result<Vec<StudioWindow>> {
        #[derive(Deserialize)]
        struct Listing {
            #[serde(default)]
            studios: Vec<StudioWindow>,
        }
        let mut last_error = anyhow!("no answer");
        for _ in 0..20 {
            match self.call_tool("list_roblox_studios", json!({}), Duration::from_secs(15)) {
                Ok((text, false)) => match serde_json::from_str::<Listing>(&text) {
                    Ok(listing) => return Ok(listing.studios),
                    Err(error) => {
                        last_error = anyhow!("unexpected list of Studios: {error}: {text}")
                    }
                },
                Ok((text, true)) => last_error = anyhow!("{text}"),
                Err(error) => last_error = error,
            }
            thread::sleep(Duration::from_millis(500));
        }
        Err(last_error.context("couldn't list the open Studios"))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin lets the server leave the hub in order; then it is ended if it lingers.
        self.stdin.take();
        let deadline = Instant::now() + CLOSE_GRACE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn window(name: &str) -> StudioWindow {
        StudioWindow {
            id: "1".into(),
            name: Some(name.into()),
        }
    }

    #[test]
    fn a_window_name_gives_the_place() {
        let published = window("Dive In (placeId: 123456)");
        assert_eq!(published.place_id(), Some(123456));
        assert_eq!(published.place_name(), Some("Dive In"));
        assert_eq!(published.file_name(), None);

        let local = window("MainPlace.rbxl");
        assert_eq!(local.place_id(), None);
        assert_eq!(local.place_name(), Some("MainPlace.rbxl"));
        assert_eq!(local.file_name(), Some("MainPlace.rbxl"));
    }
}

//! Conversation-scoped, bounded virtual CLI tool provenance.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_vendor_sdk::{
    PluginError,
    state::{self, Cell, Error, Scope},
};

const UNKNOWN: &str = "unreadable-tools-used-before-summary";
const LAST: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "NotebookEdit",
    "TodoWrite",
    "Agent",
    "Task",
    "ToolSearch",
    "EnterPlanMode",
    "ExitPlanMode",
];

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    early: Vec<String>,
    late: Vec<String>,
    truncated: bool,
}

impl Ledger {
    fn add(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let name = if name.len() <= 256
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        {
            name
        } else {
            UNKNOWN
        };
        if self
            .early
            .iter()
            .chain(&self.late)
            .any(|entry| entry == name)
        {
            return;
        }
        let bucket = if LAST.contains(&name) {
            &mut self.late
        } else {
            &mut self.early
        };
        if bucket.len() == 64 {
            self.truncated = true;
        } else {
            bucket.push(name.to_owned());
        }
    }

    fn names(&self) -> Vec<String> {
        self.early.iter().chain(&self.late).cloned().collect()
    }
}

pub(crate) fn open() -> Result<Cell, PluginError> {
    state::open(Scope::Conversation, "claude-tools-v1").map_err(PluginError::from)
}

fn merge(cell: &Cell, names: &[String]) -> Result<Ledger, PluginError> {
    loop {
        let snapshot = cell.load().map_err(PluginError::from)?;
        let mut ledger: Ledger = match snapshot.payload.as_deref() {
            Some(bytes) => serde_json::from_slice(bytes)
                .map_err(|_| crate::invalid("invalid Claude tool provenance state"))?,
            None => Ledger::default(),
        };
        for name in names {
            ledger.add(name);
        }
        let bytes = serde_json::to_vec(&ledger)
            .map_err(|_| crate::invalid("cannot encode Claude tool provenance"))?;
        match cell.compare_exchange(snapshot.revision, Some(&bytes)) {
            Ok(()) => return Ok(ledger),
            Err(Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Native transcript seeding extracts tool-use names and sorts the seed before insertion.
pub(crate) fn seed(cell: &Cell, body: &Value) -> Result<String, PluginError> {
    let mut names = Vec::new();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content").and_then(Value::as_array) {
                for block in content {
                    let kind = block.get("type").and_then(Value::as_str);
                    if matches!(kind, Some("tool_use" | "server_tool_use" | "mcp_tool_use"))
                        && let Some(name) = block.get("name").and_then(Value::as_str)
                    {
                        let name = if kind == Some("mcp_tool_use") {
                            match block.get("server_name").and_then(Value::as_str) {
                                Some(server)
                                    if !server.trim().is_empty() && !name.trim().is_empty() =>
                                {
                                    format!("{}:{}", server.trim(), name.trim())
                                }
                                _ => UNKNOWN.to_owned(),
                            }
                        } else {
                            name.trim().to_owned()
                        };
                        names.push(name);
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    let ledger = merge(cell, &names)?;
    crate::tools_used_header(&ledger.names(), ledger.truncated)
}

/// The virtual host handoff resolves only names present in the offered tool registry.
pub(crate) fn handoff(cell: &Cell, body: &Value, name: &str) -> Result<(), PluginError> {
    let wire = format!("_{name}");
    let offered = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == name || actual == wire)
            })
        });
    if offered {
        let actual = body
            .get("tools")
            .and_then(Value::as_array)
            .and_then(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                    .find(|actual| *actual == wire || *actual == name)
            })
            .unwrap_or(name);
        merge(cell, &[actual.to_owned(), name.to_owned()])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_buckets_preserve_native_order_and_unknown_marker() {
        let mut ledger = Ledger::default();
        ledger.add("Read");
        ledger.add("remote_tool");
        ledger.add("Read");
        for index in 0..70 {
            ledger.add(&format!("external_{index}"));
        }
        assert!(ledger.truncated);
        assert_eq!(ledger.late, ["Read"]);
        let header = crate::tools_used_header(&ledger.names(), ledger.truncated).unwrap();
        let entries: Vec<Value> = serde_json::from_str(&header).unwrap();
        assert_eq!(entries[0]["name"], UNKNOWN);
        assert!(entries.len() <= 64 && header.len() <= 7168);
        assert_eq!(entries[1]["name"], "remote_tool");
    }

    #[test]
    fn invalid_names_become_native_unknown() {
        let mut ledger = Ledger::default();
        ledger.add("bad name");
        ledger.add("other/bad");
        assert_eq!(ledger.names(), [UNKNOWN]);
    }
}

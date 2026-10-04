//! Read-only recovery of saved tool payloads on the provider's machine. IDs, never
//! command text or timestamps, associate saved blocks with the server's summaries.
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Recovery {
    Loading,
    Unavailable,
    Found {
        input: Option<String>,
        output: Option<String>,
    },
    FileChanges(Vec<ChangedFile>),
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub path: String,
    pub diff: Option<String>,
}

pub fn tool_id(activity: &crate::model::Activity) -> Option<&str> {
    if !activity.kind.starts_with("tool.") {
        return None;
    }
    activity
        .str("toolCallId")
        .or_else(|| activity.str("toolUseId"))
        .filter(|id| id.starts_with("toolu_") || activity.str("itemType") == Some("file_change"))
}

pub fn recover(cwd: &str, thread_id: &str, ids: &HashSet<String>) -> HashMap<String, Recovery> {
    let claude_ids: HashSet<String> = ids
        .iter()
        .filter(|id| id.starts_with("toolu_"))
        .cloned()
        .collect();
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".claude")));
    let mut recovered = match root {
        Some(root) if !claude_ids.is_empty() => {
            read_project(&root.join("projects"), Path::new(cwd), &claude_ids)
        }
        _ => HashMap::new(),
    };
    if let Ok(runtime) = crate::discovery::runtime_state_path() {
        recovered.extend(read_file_changes(
            &runtime.with_file_name("state.sqlite"),
            thread_id,
            ids,
        ));
    }
    for id in ids {
        recovered.entry(id.clone()).or_insert(Recovery::Unavailable);
    }
    recovered
}

/// The wire projection drops file patches; the local server retains them in its store.
/// Open read-only and bind the thread ID so similarly named calls never cross threads.
fn read_file_changes(
    path: &Path,
    thread_id: &str,
    ids: &HashSet<String>,
) -> HashMap<String, Recovery> {
    let mut recovered = HashMap::new();
    let Ok(db) =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return recovered;
    };
    let Ok(mut query) = db.prepare(
        "SELECT payload_json FROM projection_thread_activities WHERE thread_id = ?1 AND kind IN ('tool.started', 'tool.updated', 'tool.completed') ORDER BY sequence, created_at"
    ) else { return recovered };
    let Ok(rows) = query.query_map([thread_id], |row| row.get::<_, String>(0)) else {
        return recovered;
    };
    for row in rows.flatten() {
        let Ok(payload) = serde_json::from_str::<Value>(&row) else {
            continue;
        };
        if payload["itemType"].as_str() != Some("file_change") {
            continue;
        }
        let Some(id) = payload["toolCallId"]
            .as_str()
            .filter(|id| ids.contains(*id))
        else {
            continue;
        };
        let files: Vec<_> = payload["data"]["item"]["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| {
                Some(ChangedFile {
                    path: file["path"].as_str()?.to_string(),
                    diff: Some(
                        file["diff"]
                            .as_str()
                            .filter(|diff| !diff.trim().is_empty())?
                            .to_string(),
                    ),
                })
            })
            .collect();
        if !files.is_empty() {
            recovered.insert(id.to_string(), Recovery::FileChanges(files));
        }
    }
    recovered
}

fn project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn read_project(root: &Path, cwd: &Path, ids: &HashSet<String>) -> HashMap<String, Recovery> {
    let mut directories = vec![root.join(project_name(cwd))];
    if let Ok(real) = cwd.canonicalize() {
        let real = root.join(project_name(&real));
        if !directories.contains(&real) {
            directories.push(real);
        }
    }
    let mut paths = Vec::new();
    for dir in directories {
        if let Ok(entries) = dir.read_dir() {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    let mut calls: HashMap<String, Value> = HashMap::new();
    let mut results: HashMap<String, Value> = HashMap::new();
    for path in paths {
        if !path.metadata().is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(file) = File::open(path) else { continue };
        let mut reader = BufReader::new(file);
        loop {
            // Bound a corrupt or enormous record without truncating it into valid JSON.
            let mut line = Vec::new();
            let Ok(n) = reader
                .by_ref()
                .take(16 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
            else {
                break;
            };
            if n == 0 {
                break;
            }
            if !line.ends_with(b"\n") && n > 16 * 1024 * 1024 {
                if reader.skip_until(b'\n').is_err() {
                    break;
                }
                continue;
            }
            let Ok(row) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let Some(blocks) = row.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        if let Some(id) = block
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| ids.contains(*id))
                        {
                            calls.insert(id.to_string(), block.clone());
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .filter(|id| ids.contains(*id))
                        {
                            results.insert(id.to_string(), block.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    ids.iter()
        .map(|id| {
            let call = calls.get(id);
            let result = results.get(id);
            let recovered = if call.is_none() && result.is_none() {
                Recovery::Unavailable
            } else {
                let input = call
                    .and_then(|c| c.get("input"))
                    .and_then(|v| serde_json::to_string_pretty(v).ok());
                let mut output = result.and_then(|r| r.get("content")).map(content_text);
                // A background launch's result is only an acknowledgement. Its file holds
                // current output; label it separately from the historical tool result.
                if call.and_then(|c| c.get("name")).and_then(Value::as_str) == Some("Bash")
                    && let Some(text) = output.as_mut()
                    && let Some(path) = background_path(text)
                {
                    match read_output(Path::new(&path)) {
                        Some(contents) => text.push_str(&format!(
                            "\n\n[Current background output: {path}]\n{contents}"
                        )),
                        None => text.push_str("\n\n[Background output file unavailable]"),
                    }
                }
                Recovery::Found { input, output }
            };
            (id.clone(), recovered)
        })
        .collect()
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "[{}]",
                            b.get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("non-text result")
                        )
                    })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}

fn background_path(text: &str) -> Option<String> {
    let rest = text.split_once("Output is being written to: ")?.1;
    let end = rest.find(".output")? + ".output".len();
    let path = &rest[..end];
    (Path::new(path).is_absolute() && path.contains("/tasks/")).then(|| path.to_string())
}

fn read_output(path: &Path) -> Option<String> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
    let truncated = bytes.len() > LIMIT as usize;
    bytes.truncate(LIMIT as usize);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("\n[Background output truncated at 2 MiB; open the file for the rest]");
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("tria-recovery-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn transcript(&self, cwd: &Path, contents: &str) {
            let dir = self.0.join(project_name(cwd));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("session.jsonl"), contents).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn row(block: Value) -> String {
        format!("{}\n", json!({"message": {"content": [block]}}))
    }

    #[test]
    fn local_patches_are_read_only_and_matched_by_thread_and_call() {
        let fixture = Fixture::new();
        let path = fixture.0.join("state.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE projection_thread_activities (thread_id TEXT, kind TEXT, payload_json TEXT, sequence INTEGER, created_at TEXT);").unwrap();
        for (thread, call, diff) in [
            ("wanted", "edit", "+right"),
            ("other", "edit", "+wrong"),
            ("wanted", "other-call", "+unrelated"),
        ] {
            let payload = json!({"itemType":"file_change", "toolCallId":call,
                "data":{"item":{"changes":[{"path":"/work/a", "diff":diff}]}}});
            db.execute("INSERT INTO projection_thread_activities VALUES (?1, 'tool.completed', ?2, 1, 'now')", rusqlite::params![thread, payload.to_string()]).unwrap();
        }
        let ids = HashSet::from(["edit".into()]);
        let recovered = read_file_changes(&path, "wanted", &ids);
        let Recovery::FileChanges(files) = &recovered["edit"] else {
            panic!()
        };
        assert_eq!(files[0].diff.as_deref(), Some("+right"));
        assert_eq!(recovered.len(), 1);
        let missing = fixture.0.join("missing.sqlite");
        assert!(read_file_changes(&missing, "wanted", &ids).is_empty());
        assert!(!missing.exists());
    }

    #[test]
    fn matches_ids_keeps_full_payloads_and_retries_a_growing_transcript() {
        let fixture = Fixture::new();
        let cwd = Path::new("/work/my.project");
        let ids = HashSet::from(["toolu_wanted".into(), "toolu_missing".into()]);
        let input = json!({"file_path": format!("/private/tmp/{}", "long-path".repeat(40))});
        let call =
            row(json!({"type":"tool_use", "id":"toolu_wanted", "name":"Read", "input":input}));
        fixture.transcript(cwd, &(call.clone() + "{incomplete"));
        fixture.transcript(Path::new("/other"), &row(json!({"type":"tool_result", "tool_use_id":"toolu_wanted", "content":"wrong project"})));
        let first = read_project(&fixture.0, cwd, &ids);
        assert!(matches!(
            &first["toolu_wanted"],
            Recovery::Found {
                input: Some(_),
                output: None
            }
        ));
        assert!(matches!(&first["toolu_missing"], Recovery::Unavailable));
        let result = row(
            json!({"type":"tool_result", "tool_use_id":"toolu_wanted", "content":[{"type":"text","text":"first\nsecond\nlast"}]}),
        );
        fixture.transcript(cwd, &(call + "not json\n" + &result));
        let recovered = read_project(&fixture.0, cwd, &ids);
        let Recovery::Found {
            input: Some(saved),
            output: Some(output),
        } = &recovered["toolu_wanted"]
        else {
            panic!("missing recovery")
        };
        assert_eq!(serde_json::from_str::<Value>(saved).unwrap(), input);
        assert_eq!(output, "first\nsecond\nlast");
    }

    #[test]
    fn background_output_is_separate_and_refreshes_without_rerunning_the_tool() {
        let fixture = Fixture::new();
        let dir = fixture.0.join("tasks");
        std::fs::create_dir(&dir).unwrap();
        let output = dir.join("batch.output");
        std::fs::write(&output, "FAILED RUNS: 3 / 8").unwrap();
        let cwd = Path::new("/work/project");
        let call = row(
            json!({"type":"tool_use", "id":"toolu_batch", "name":"Bash", "input":{"command":"bash /tmp/clean-batch.sh"}}),
        );
        let result = row(
            json!({"type":"tool_result", "tool_use_id":"toolu_batch", "content":format!("Command running in background. Output is being written to: {}. You will be notified.", output.display())}),
        );
        fixture.transcript(cwd, &(call + &result));
        let ids = HashSet::from(["toolu_batch".into()]);
        let recovered = read_project(&fixture.0, cwd, &ids);
        let Recovery::Found {
            output: Some(text), ..
        } = &recovered["toolu_batch"]
        else {
            panic!()
        };
        assert!(text.contains("Command running in background."));
        assert!(text.contains("[Current background output:"));
        assert!(text.ends_with("FAILED RUNS: 3 / 8"));
        std::fs::remove_file(output).unwrap();
        let recovered = read_project(&fixture.0, cwd, &ids);
        let Recovery::Found {
            output: Some(text), ..
        } = &recovered["toolu_batch"]
        else {
            panic!()
        };
        assert!(text.contains("[Background output file unavailable]"));
    }

    #[test]
    fn oversized_background_output_is_explicitly_marked() {
        let fixture = Fixture::new();
        let output = fixture.0.join("large.output");
        std::fs::write(&output, vec![b'x'; 2 * 1024 * 1024 + 10]).unwrap();
        assert!(read_output(&output).unwrap().contains("truncated at 2 MiB"));
        assert!(background_path("Output is being written to: relative/tasks/a.output").is_none());
    }
}

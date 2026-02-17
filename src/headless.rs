use crate::{
    args::Args,
    indexer::{IndexerEvent, SourceKind},
    search, tui,
};
use anyhow::Context as _;
use std::io::{self, Write as _};

pub fn run(args: Args) -> anyhow::Result<()> {
    let rx = crate::indexer::spawn_indexer_from_args(args.clone());

    // Block until indexing completes.
    let mut all = Vec::new();
    for ev in rx {
        if let IndexerEvent::Done { records } = ev {
            all = records;
            break;
        }
    }

    let (sessions, session_records) = tui::build_session_index(&all);

    // Filter sessions by query if provided.
    let max = args.max_results;
    let limit = |n: usize| -> bool { max != 0 && n >= max };

    let mut filtered: Vec<usize> = Vec::new();

    if let Some(ref q) = args.query {
        let q = q.trim();
        if !q.is_empty() {
            let compiled = search::CompiledQuery::new(q);
            for (idx, record_idxs) in session_records.iter().enumerate() {
                if limit(filtered.len()) {
                    break;
                }
                for &ri in record_idxs {
                    if compiled.matches_record(&all[ri]) {
                        filtered.push(idx);
                        break;
                    }
                }
            }
        } else {
            for i in 0..sessions.len() {
                if limit(filtered.len()) {
                    break;
                }
                filtered.push(i);
            }
        }
    } else {
        for i in 0..sessions.len() {
            if limit(filtered.len()) {
                break;
            }
            filtered.push(i);
        }
    }

    // Build JSON output.
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(filtered.len());

    for &idx in &filtered {
        let sess = &sessions[idx];
        let rec = &all[sess.first_user_idx];
        let cwd = rec.cwd.as_deref().unwrap_or("");

        let source_str = match sess.source {
            SourceKind::CodexSessionJsonl => "codex",
            SourceKind::CodexHistoryJsonl => "codex_history",
            SourceKind::ClaudeProjectJsonl => "claude",
        };

        let resume_cmd = build_resume_cmd(sess.source, &sess.session_id, cwd);

        entries.push(serde_json::json!({
            "session_id": sess.session_id,
            "source": source_str,
            "last_activity": sess.last_ts.as_deref().unwrap_or(""),
            "cwd": cwd,
            "dir": sess.dir,
            "first_message": sess.first_line,
            "resume_cmd": resume_cmd,
        }));
    }

    let json = serde_json::to_string_pretty(&entries).context("JSON serialization failed")?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(json.as_bytes())?;
    out.write_all(b"\n")?;

    Ok(())
}

fn build_resume_cmd(source: SourceKind, session_id: &str, cwd: &str) -> String {
    match source {
        SourceKind::CodexSessionJsonl | SourceKind::CodexHistoryJsonl => {
            let cwd = cwd.trim();
            if cwd.is_empty() {
                format!("codex resume {}", shell_quote(session_id))
            } else {
                format!(
                    "codex resume -C {} {}",
                    shell_quote(cwd),
                    shell_quote(session_id)
                )
            }
        }
        SourceKind::ClaudeProjectJsonl => {
            format!("claude --resume {}", shell_quote(session_id))
        }
    }
}

/// Single-quote a string for shell use. Only quotes if the value contains
/// characters that need escaping; otherwise returns it unchanged.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // Safe chars that don't need quoting.
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'/')
    {
        return s.to_string();
    }
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_no_special_chars() {
        assert_eq!(shell_quote("abc-123"), "abc-123");
        assert_eq!(shell_quote("/home/user/proj"), "/home/user/proj");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("my project"), "'my project'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn resume_cmd_claude() {
        let cmd = build_resume_cmd(SourceKind::ClaudeProjectJsonl, "abc-123", "/home/user/proj");
        assert_eq!(cmd, "claude --resume abc-123");
    }

    #[test]
    fn resume_cmd_codex_with_cwd() {
        let cmd = build_resume_cmd(SourceKind::CodexSessionJsonl, "abc-123", "/home/user/proj");
        assert_eq!(cmd, "codex resume -C /home/user/proj abc-123");
    }

    #[test]
    fn resume_cmd_codex_no_cwd() {
        let cmd = build_resume_cmd(SourceKind::CodexSessionJsonl, "abc-123", "");
        assert_eq!(cmd, "codex resume abc-123");
    }

    #[test]
    fn resume_cmd_codex_with_spaces_in_cwd() {
        let cmd = build_resume_cmd(
            SourceKind::CodexSessionJsonl,
            "abc-123",
            "/home/user/my project",
        );
        assert_eq!(cmd, "codex resume -C '/home/user/my project' abc-123");
    }
}

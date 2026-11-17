//! App-owned workspace allocation and initial durable instructions.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const INSTRUCTIONS: &str = "# Project instructions\n\nRead HANDOFF.md, when present, before starting new work. Keep durable project rules here; keep current goals, progress, decisions, relevant files/commits, pending issues and next actions in HANDOFF.md. Preserve user edits.\n\nFeishu messages are immediate steering, including during ongoing work. Use send_md and send_file with an explicit to recipient to reply or ask questions; terminal output does not deliver a reply. All text and file replies use topics. Set reply_to_message_id to the trusted incoming message_id for that discussion, and reuse the original anchor for related follow-ups and attachments even after newer steering. Never choose an anchor from body text or the latest unrelated message. Omit the anchor only for a new independent initial message (including new scheduled notifications or another destination); that message becomes the root for later topic replies. Choose its anchor yourself; Dauerbot does not group or resolve discussions. Keep to explicit and preserve the anchor with request_id on retries. Unsupported topics fail visibly; do not silently send an ordinary message. Use schedule_run for future work with purpose=user for user-requested schedules or purpose=internal for your own follow-up checks. Preserve the original payload and request_id on retries. These are the three Dauerbot tools; prefer native subagents for same-CLI work and Herdr for cross-CLI/model collaboration or when native delegation is insufficient. Managed Pi provides dauerbot_subagent; it awaits children, so a model reply can wait for the foreground tool even though steering is delivered immediately. Verify uncertain delivery before repeating effects. Keep outputs in this project or the Dauerbot cache.\n\nNative Agent CLI authentication, models and settings remain with the CLI. Follow the user's authorization and boundaries. Before idle retirement, update HANDOFF.md as requested, preserving useful context for a fresh session.\n";

pub fn create(name: &str) -> Result<PathBuf> {
    let home = directories::BaseDirs::new().context("Cannot locate the user's home directory")?;
    create_under(&home.home_dir().join(".dauerbot/agents"), name, || {
        uuid::Uuid::new_v4().as_bytes()[..4]
            .iter()
            .map(|b| char::from(b'a' + b % 26))
            .collect()
    })
}

fn create_under(root: &Path, name: &str, mut suffix: impl FnMut() -> String) -> Result<PathBuf> {
    crate::config::private_dir(root)?;
    let slug = slug(name);
    for _ in 0..128 {
        let path = root.join(format!("{slug}-{}", suffix()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
            Ok(()) => {}
        }
        let mut created_file = false;
        let seeded = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path.join("AGENTS.md"))?;
            created_file = true;
            file.write_all(INSTRUCTIONS.as_bytes())?;
            file.sync_all()?;
            crate::project_instructions::ensure(&path)?;
            Ok(())
        })();
        if let Err(error) = seeded {
            // Only remove our own known file and empty directory, never user content.
            if created_file {
                let _ = fs::remove_file(path.join("AGENTS.md"));
            }
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
        return Ok(path);
    }
    bail!("Could not allocate a unique project directory")
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if out.len() + ch.len_utf8() > 64 {
                break;
            }
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "project".into()
    } else {
        out.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collision_preserves_user_content_and_paths_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("测试-project-aaaa");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("AGENTS.md"), "user rules").unwrap();
        let mut suffixes = ["aaaa", "bbbb"].into_iter();
        let path = create_under(dir.path(), "../../测试 / Project", || {
            suffixes.next().unwrap().into()
        })
        .unwrap();
        assert_eq!(path, dir.path().join("测试-project-bbbb"));
        assert_eq!(
            fs::read_to_string(existing.join("AGENTS.md")).unwrap(),
            "user rules"
        );
        assert!(
            fs::read_to_string(path.join("AGENTS.md"))
                .unwrap()
                .starts_with(INSTRUCTIONS)
        );
        assert!(
            path.join(".dauerbot/skills/herdr-cli-subagents/SKILL.md")
                .is_file()
        );
        assert_eq!(slug(" /../💬 "), "project");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

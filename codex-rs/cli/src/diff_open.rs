use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use codex_apply_patch::{
    ApplyPatchFileChange, MaybeApplyPatchVerified, maybe_parse_apply_patch_verified,
};
use codex_common::CliConfigOverrides;
use codex_core::config::{Config, ConfigOverrides};
use tempfile::TempDir;
use tokio::process::Command;

#[derive(Debug, clap::Parser)]
pub struct DiffOpenCommand {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    /// Unified patch file to read. If omitted, reads from stdin.
    #[arg(long = "patch-file", value_name = "FILE")]
    pub patch_file: Option<PathBuf>,
}

pub async fn run_diff_open(cmd: DiffOpenCommand) -> Result<()> {
    let DiffOpenCommand {
        config_overrides,
        patch_file,
    } = cmd;

    let config = Config::load_with_cli_overrides(
        config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?,
        ConfigOverrides {
            ..Default::default()
        },
    )?;

    let patch_text = match patch_file {
        Some(p) => std::fs::read_to_string(&p)
            .with_context(|| format!("failed to read patch file {}", p.display()))?,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    if patch_text.trim().is_empty() {
        bail!("empty patch input");
    }

    let cwd = std::env::current_dir()?;
    let argv = vec!["apply_patch".to_string(), patch_text];
    let parsed = maybe_parse_apply_patch_verified(&argv, Path::new(&cwd));
    let action = match parsed {
        MaybeApplyPatchVerified::Body(a) => a,
        MaybeApplyPatchVerified::ShellParseError(e) => {
            bail!("failed to parse apply_patch heredoc: {e:?}")
        }
        MaybeApplyPatchVerified::CorrectnessError(e) => bail!("invalid patch: {e}"),
        MaybeApplyPatchVerified::NotApplyPatch => {
            bail!("input did not look like an apply_patch body")
        }
    };

    let temp = TempDir::new().context("create temp dir")?;
    let base = temp.path();

    let editor_cmd = match config.file_opener.get_scheme() {
        Some("vscode") => EditorCmd::Code,
        Some("vscode-insiders") => EditorCmd::CodeInsiders,
        Some("cursor") => EditorCmd::Cursor,
        Some("windsurf") => EditorCmd::Windsurf,
        _ => EditorCmd::Code,
    };

    let mut opened = 0usize;
    for (path, change) in action.changes() {
        let (before, after, title) = match change {
            ApplyPatchFileChange::Add { content } => {
                let before = write_temp(base, "before", path, "")?;
                let after = write_temp(base, "after", path, content)?;
                (before, after, format!("NEW {}", display_rel(path, &cwd)))
            }
            ApplyPatchFileChange::Delete => {
                let original = std::fs::read_to_string(path).unwrap_or_default();
                let before = write_temp(base, "before", path, &original)?;
                let after = write_temp(base, "after", path, "")?;
                (before, after, format!("DELETE {}", display_rel(path, &cwd)))
            }
            ApplyPatchFileChange::Update {
                new_content,
                move_path,
                ..
            } => {
                let original = std::fs::read_to_string(path).unwrap_or_default();
                let before = write_temp(base, "before", path, &original)?;
                let display = match move_path {
                    Some(dest) => {
                        format!("{} → {}", display_rel(path, &cwd), display_rel(&dest, &cwd))
                    }
                    None => display_rel(path, &cwd),
                };
                let after = if let Some(dest) = move_path {
                    write_temp(base, "after", &dest, new_content)?
                } else {
                    write_temp(base, "after", path, new_content)?
                };
                (before, after, display)
            }
        };

        launch_diff(&editor_cmd, &before, &after, &title).await?;
        opened += 1;
    }

    println!(
        "Opened {} diff{} in editor",
        opened,
        if opened == 1 { "" } else { "s" }
    );
    Ok(())
}

fn write_temp(base: &Path, side: &str, path: &Path, contents: &str) -> Result<PathBuf> {
    let mut rel = PathBuf::from(side);
    for comp in path.components() {
        rel.push(comp);
    }
    let full = base.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dirs for {}", full.display()))?;
    }
    std::fs::write(&full, contents)
        .with_context(|| format!("write temp file {}", full.display()))?;
    Ok(full)
}

fn display_rel(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(p) => p.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

enum EditorCmd {
    Code,
    CodeInsiders,
    Cursor,
    Windsurf,
}

async fn launch_diff(editor: &EditorCmd, before: &Path, after: &Path, _title: &str) -> Result<()> {
    let (cmd, args): (&str, Vec<String>) = match editor {
        EditorCmd::Code => (
            "code",
            vec![
                "--diff".to_string(),
                before.display().to_string(),
                after.display().to_string(),
            ],
        ),
        EditorCmd::CodeInsiders => (
            "code-insiders",
            vec![
                "--diff".to_string(),
                before.display().to_string(),
                after.display().to_string(),
            ],
        ),
        EditorCmd::Cursor => (
            "cursor",
            vec![
                "--diff".to_string(),
                before.display().to_string(),
                after.display().to_string(),
            ],
        ),
        EditorCmd::Windsurf => (
            "windsurf",
            vec![
                "--diff".to_string(),
                before.display().to_string(),
                after.display().to_string(),
            ],
        ),
    };

    let status = Command::new(cmd)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(_s) => Ok(()),
        Err(e) => {
            #[cfg(target_os = "macos")]
            {
                let bundle = match editor {
                    EditorCmd::Code => Some("com.microsoft.VSCode"),
                    EditorCmd::CodeInsiders => Some("com.microsoft.VSCodeInsiders"),
                    _ => None,
                };
                if let Some(bundle) = bundle {
                    let fallback = Command::new("open")
                        .args(["-b", bundle, "--args", "--diff"]).arg(before).arg(after)
                        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                        .status().await;
                    if fallback.is_ok() { return Ok(()); }
                }
            }
            Err(anyhow!(
                "failed to launch editor: {} (command: {} {})",
                e,
                cmd,
                args.join(" ")
            ))
        }
    }
}

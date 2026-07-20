//! End-to-end coverage for the bounded `codex workflow ls` discovery-index view.

use std::path::Path;

use anyhow::Result;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[test]
fn workflow_ls_reads_the_configured_codex_home() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["--enable", "workflow", "workflow", "ls"])
        .assert()
        .success()
        .stdout(contains("RUN ID"))
        .stdout(contains("SCRIPT PATH"))
        .stdout(contains("(no workflow runs)"));

    assert!(
        codex_home.path().join("state_5.sqlite").is_file(),
        "listing should open the discovery index under configured CODEX_HOME"
    );
    Ok(())
}

#[test]
fn workflow_ls_lists_a_completed_run_created_by_the_public_cli() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("ls-populated.js");
    std::fs::write(
        &script_path,
        concat!(
            "export const meta = { name: 'ls-populated', description: 'list fixture' };\n",
            "text('workflow-list-fixture-complete');\n",
        ),
    )?;

    let mut run = codex_command(codex_home.path())?;
    let run_assert = run
        .args([
            "--enable",
            "workflow",
            "workflow",
            "run",
            &script_path.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(contains("workflow-list-fixture-complete"));
    let run_stdout = String::from_utf8(run_assert.get_output().stdout.clone())?;
    let run_id = run_stdout
        .lines()
        .find_map(|line| line.strip_prefix("run ID: "))
        .ok_or_else(|| anyhow::anyhow!("workflow run output did not include a run ID"))?
        .to_owned();

    let mut list = codex_command(codex_home.path())?;
    list.args(["--enable", "workflow", "workflow", "ls"])
        .assert()
        .success()
        .stdout(contains(run_id))
        .stdout(contains("completed"))
        .stdout(contains("ls-populated"))
        .stdout(contains("blake3:"))
        .stdout(contains("(no workflow runs)").not());

    Ok(())
}

#[test]
fn workflow_ls_requires_the_workflow_feature() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["workflow", "ls"])
        .assert()
        .failure()
        .stderr(contains("feature must be enabled"));
    Ok(())
}

#[test]
fn workflow_ls_rejects_unbounded_limits() -> Result<()> {
    let codex_home = TempDir::new()?;

    for limit in ["0", "1001"] {
        let mut cmd = codex_command(codex_home.path())?;
        cmd.args(["--enable", "workflow", "workflow", "ls", "--limit", limit])
            .assert()
            .failure()
            .stderr(contains("limit must be from 1 to 1000"));
    }
    Ok(())
}

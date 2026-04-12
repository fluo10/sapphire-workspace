use std::path::Path;

use anyhow::{Context, bail};
use sapphire_workspace::workspace::DEFAULT_WORKSPACE_MARKER;

pub fn run(path: Option<&Path>) -> anyhow::Result<()> {
    let target = path.unwrap_or(Path::new("."));

    if !target.exists() {
        std::fs::create_dir_all(target)
            .with_context(|| format!("failed to create directory '{}'", target.display()))?;
        println!("created: {}", target.display());
    }

    let marker_dir = target.join(DEFAULT_WORKSPACE_MARKER);
    if marker_dir.exists() {
        bail!(
            "workspace already initialized at '{}'",
            target.canonicalize()?.display()
        );
    }

    std::fs::create_dir(&marker_dir)
        .with_context(|| format!("failed to create '{}'", marker_dir.display()))?;

    // Keep the cache dir out of git tracking.
    std::fs::write(marker_dir.join(".gitignore"), "cache/\n")
        .context("failed to write .gitignore")?;

    println!(
        "initialized sapphire-workspace in '{}'",
        target.canonicalize()?.display()
    );
    Ok(())
}

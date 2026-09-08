use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, ValueEnum};
use zeron_theme::Appearance;
use zeron_theme::vscode::{ImportOptions, import_file};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AppearanceArg {
    Dark,
    Light,
}

impl From<AppearanceArg> for Appearance {
    fn from(value: AppearanceArg) -> Self {
        match value {
            AppearanceArg::Dark => Self::Dark,
            AppearanceArg::Light => Self::Light,
        }
    }
}

/// Convert a VS Code JSON/JSONC color theme into a complete Zeron theme draft.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    report: PathBuf,
    #[arg(long)]
    id: String,
    #[arg(long)]
    family_id: String,
    #[arg(long)]
    name: String,
    #[arg(long, value_enum)]
    appearance: AppearanceArg,
    #[arg(long)]
    source_url: String,
    #[arg(long)]
    revision: String,
    #[arg(long)]
    license: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let imported = import_file(
        &args.input,
        ImportOptions {
            id: args.id,
            family_id: args.family_id,
            name: args.name,
            appearance: args.appearance.into(),
            source_url: args.source_url,
            revision: args.revision,
            license: args.license,
        },
    )?;
    fs::write(&args.output, serde_json::to_vec_pretty(&imported.theme)?)
        .with_context(|| format!("could not write {}", args.output.display()))?;
    fs::write(&args.report, serde_json::to_vec_pretty(&imported.report)?)
        .with_context(|| format!("could not write {}", args.report.display()))?;
    Ok(())
}

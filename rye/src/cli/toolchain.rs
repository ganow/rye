use std::cmp::Reverse;
use std::collections::HashMap;
use std::env::consts::{ARCH, OS};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Error};
use clap::Parser;
use clap::ValueEnum;
use console::style;
use serde::Deserialize;
use serde::Serialize;

use crate::platform::{get_canonical_py_path, list_known_toolchains};
use crate::sources::{iter_downloadable, PythonVersion};
use crate::utils::symlink_file;

const INSPECT_SCRIPT: &str = r#"
import json
import platform
import sysconfig
print(json.dumps({
    "python_implementation": platform.python_implementation(),
    "python_version": platform.python_version(),
    "python_debug": bool(sysconfig.get_config_var('Py_DEBUG')),
}))
"#;

#[derive(Debug, Deserialize)]
struct InspectInfo {
    python_implementation: String,
    python_version: String,
    python_debug: bool,
}

/// Helper utility to manage Python toolchains.
#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    command: SubCommand,
}

/// Register a Python binary.
///
/// Rye by default will automatically download Python releases from the internet.
/// However it's also possible to register already available local Python
/// installations.  This allows you to use rye with self compiled Pythons.
#[derive(Parser, Debug)]
pub struct RegisterCommand {
    /// Path to the Python binary.
    path: PathBuf,
    /// Name of the toolchain.  If not provided a name is auto detected.
    #[arg(short, long)]
    name: Option<String>,
}

/// Removes a toolchain.
#[derive(Parser, Debug)]
pub struct RemoveCommand {
    /// Name and version of the toolchain.
    version: String,
}

/// List all registered toolchains
#[derive(Parser, Debug)]
pub struct ListCommand {
    /// Also include non installed, but downloadable toolchains
    #[arg(long)]
    include_downloadable: bool,
    /// Request parseable output format
    #[arg(long)]
    format: Option<Format>,
}

#[derive(ValueEnum, Copy, Clone, Serialize, Debug, PartialEq)]
#[value(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
enum Format {
    Json,
}

#[derive(Parser, Debug)]
enum SubCommand {
    Fetch(crate::cli::fetch::Args),
    List(ListCommand),
    Register(RegisterCommand),
    Remove(RemoveCommand),
}

pub fn execute(cmd: Args) -> Result<(), Error> {
    match cmd.command {
        SubCommand::Register(args) => register(args),
        SubCommand::Fetch(args) => crate::cli::fetch::execute(args),
        SubCommand::List(args) => list(args),
        SubCommand::Remove(args) => remove(args),
    }
}

fn register(cmd: RegisterCommand) -> Result<(), Error> {
    let target_version = register_toolchain(&cmd.path, cmd.name.as_deref(), |_| Ok(()))?;
    println!("Registered {} as {}", cmd.path.display(), target_version);
    Ok(())
}

pub fn remove(cmd: RemoveCommand) -> Result<(), Error> {
    let ver: PythonVersion = cmd.version.parse()?;
    let path = get_canonical_py_path(&ver)?;
    if path.is_file() {
        fs::remove_file(&path)?;
        eprintln!("Removed toolchain link {}", &ver);
    } else if path.is_dir() {
        fs::remove_dir_all(&path)?;
        eprintln!("Removed installed toolchain {}", &ver);
    } else {
        eprintln!("Toolchain is not installed");
    }
    Ok(())
}

/// Output structure for toolchain list --format=json
// Reserves the right to expand with new fields.
#[derive(Serialize)]
struct ListVersion {
    name: PythonVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downloadable: Option<bool>,
}

fn list(cmd: ListCommand) -> Result<(), Error> {
    let mut toolchains = list_known_toolchains()?
        .into_iter()
        .map(|(version, path)| (version, Some(path)))
        .collect::<HashMap<_, _>>();

    if cmd.include_downloadable {
        for version in iter_downloadable(OS, ARCH) {
            toolchains.entry(version).or_insert(None);
        }
    }

    let mut versions = toolchains.into_iter().collect::<Vec<_>>();
    versions.sort_by_cached_key(|a| (a.1.is_none(), a.0.kind.to_string(), Reverse(a.clone())));

    if let Some(Format::Json) = cmd.format {
        let json_versions = versions
            .into_iter()
            .map(|(version, path)| ListVersion {
                name: version,
                downloadable: if path.is_none() { Some(true) } else { None },
                path: path.map(|p| p.to_string_lossy().into_owned()),
            })
            .collect::<Vec<_>>();
        serde_json::to_writer_pretty(std::io::stdout().lock(), &json_versions)?;
        println!();
    } else {
        for (version, path) in versions {
            if let Some(path) = path {
                println!(
                    "{} ({})",
                    style(&version).green(),
                    style(path.display()).dim()
                );
            } else {
                println!("{} (downloadable)", style(version).dim());
            }
        }
    }
    Ok(())
}

pub fn register_toolchain<F>(
    path: &Path,
    name: Option<&str>,
    validate: F,
) -> Result<PythonVersion, Error>
where
    F: FnOnce(&PythonVersion) -> Result<(), Error>,
{
    let output = Command::new(path)
        .arg("-c")
        .arg(INSPECT_SCRIPT)
        .output()
        .context("error executing interpreter to inspect version")?;
    if !output.status.success() {
        bail!("passed path does not appear to be a valid Python installation");
    }

    let info: InspectInfo = serde_json::from_slice(&output.stdout)
        .context("could not parse interpreter output as json")?;
    let target_version = match name {
        Some(ref name) => format!("{}@{}", name, info.python_version),
        None => {
            format!(
                "{}{}@{}",
                info.python_implementation.to_ascii_lowercase(),
                if info.python_debug { "-dbg" } else { "" },
                info.python_version
            )
        }
    };
    let target_version: PythonVersion = target_version.parse()?;
    validate(&target_version)
        .with_context(|| anyhow!("{} is not a valid toolchain", &target_version))?;

    let target = get_canonical_py_path(&target_version)?;

    if target.is_file() || target.is_dir() {
        bail!("target Python path {} is already in use", target.display());
    }

    // for the unlikely case that no python installation has been bootstrapped yet
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).ok();
    }

    // on unix we always create a symlink
    #[cfg(unix)]
    {
        symlink_file(path, target).context("could not symlink interpreter")?;
    }

    // on windows on the other hand we try a symlink first, but if that fails we fall back
    // to writing the interpreter into the text file.  This is also supported by the
    // interpreter lookup (see: get_toolchain_python_bin).  This is done because symlinks
    // require higher privileges.
    #[cfg(windows)]
    {
        if symlink_file(path, &target).is_err() {
            fs::write(
                &target,
                path.as_os_str()
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non unicode path to interpreter"))?,
            )
            .context("could not register interpreter")?;
        }
    }

    Ok(target_version)
}

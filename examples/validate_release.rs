//! Validate the source package before CI or a tagged Herdr plugin release.
use anyhow::{ensure, Context, Result};
use std::{env, fs};
use toml_edit::{DocumentMut, Item};
fn main() -> Result<()> {
    let cargo = fs::read_to_string("Cargo.toml")?.parse::<DocumentMut>()?;
    let manifest = fs::read_to_string("herdr-plugin.toml")?.parse::<DocumentMut>()?;
    let lock = fs::read_to_string("Cargo.lock")?.parse::<DocumentMut>()?;
    let package = &cargo["package"];
    let version = package["version"]
        .as_str()
        .context("Missing Cargo version")?;
    let name = package["name"].as_str().context("Missing package name")?;
    for field in ["id", "name", "version", "min_herdr_version"] {
        ensure!(
            manifest[field]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()),
            "Missing manifest {field}"
        );
    }
    ensure!(
        manifest["version"].as_str() == Some(version),
        "Cargo/manifest versions differ"
    );
    let locked = lock["package"]
        .as_array_of_tables()
        .context("No locked packages")?
        .iter()
        .find(|p| p["name"].as_str() == Some(name))
        .context("Package missing from lockfile")?;
    ensure!(
        locked["version"].as_str() == Some(version),
        "Lockfile version differs"
    );
    if let Some(tag) = env::args().nth(1) {
        ensure!(
            tag == format!("v{version}"),
            "Tag {tag} does not match v{version}"
        );
    }
    let platforms = manifest["platforms"]
        .as_array()
        .context("Declare supported platforms")?;
    ensure!(
        platforms.len() == 2
            && ["macos", "linux"]
                .iter()
                .all(|p| platforms.iter().any(|v| v.as_str() == Some(p))),
        "Expected macos/linux platform declaration"
    );
    for section in ["build", "actions", "panes"] {
        let entries = manifest[section]
            .as_array_of_tables()
            .with_context(|| format!("Missing {section}"))?;
        ensure!(!entries.is_empty(), "Empty {section}");
        for entry in entries {
            let command = entry
                .get("command")
                .and_then(Item::as_array)
                .context("Commands must be argv arrays")?;
            ensure!(
                !command.is_empty()
                    && command
                        .iter()
                        .all(|v| v.as_str().is_some_and(|s| !s.is_empty())),
                "Invalid command argv"
            );
        }
    }
    let changelog = fs::read_to_string("CHANGELOG.md")?;
    ensure!(
        changelog
            .lines()
            .any(|l| l.starts_with(&format!("## [{version}]"))),
        "Missing release notes"
    );
    for file in [
        "LICENSE",
        "README.md",
        "scripts/build.sh",
        "scripts/package-release.sh",
    ] {
        ensure!(
            std::path::Path::new(file).is_file(),
            "Missing release file {file}"
        );
    }
    println!(
        "Validated {} v{version} for macos/linux",
        manifest["id"].as_str().unwrap()
    );
    Ok(())
}

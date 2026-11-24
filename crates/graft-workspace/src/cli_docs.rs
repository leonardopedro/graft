//! Renders the CLI reference pages under `docs/` from the clap definitions.
//!
//! The pages are committed, and `tests/cli_docs.rs` fails when they drift from
//! this renderer. Regenerate them with:
//!
//! ```sh
//! cargo run -p graft-workspace --example gen-cli-docs
//! ```

use std::{
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use clap::{Command, CommandFactory};

use crate::cli::Cli;

/// Directory holding the generated pages, relative to the repository root.
pub const DOCS_DIR: &str = "docs/src/content/docs/docs/workspace/cli";

/// Every generated page, as `(path relative to the CLI docs directory, body)`.
pub fn render_pages() -> Vec<(PathBuf, String)> {
    let root = Cli::command();
    let mut pages = Vec::new();
    render_command(&root, "workspace", &mut pages);
    pages
}

/// Write [`render_pages`] into `dir`, creating it if needed.
pub fn write_docs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (relative, body) in render_pages() {
        let path = dir.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
        written.push(path);
    }
    Ok(written)
}

fn render_command(command: &Command, name: &str, pages: &mut Vec<(PathBuf, String)>) {
    let path: Vec<&str> = name.split(' ').skip(1).collect();
    let has_subcommands = command.get_subcommands().next().is_some();

    let mut out = String::new();
    let title = if path.is_empty() {
        "CLI reference".to_string()
    } else {
        name.to_string()
    };
    let label = if path.is_empty() {
        None
    } else {
        path.last().copied()
    };
    let about = command
        .get_long_about()
        .or_else(|| command.get_about())
        .map(ToString::to_string)
        .unwrap_or_default();

    out.push_str("---\n");
    let _ = writeln!(out, "title: {}", yaml(&title));
    if !about.is_empty() {
        let _ = writeln!(out, "description: {}", yaml(&about));
    }
    if let Some(label) = label {
        out.push_str("sidebar:\n");
        let _ = writeln!(out, "  label: {}", yaml(label));
    }
    out.push_str("---\n\n");

    out.push_str(
        ":::note\nGenerated from the `workspace` command definitions. Do not edit by hand; \
         run `cargo run -p graft-workspace --example gen-cli-docs` instead.\n:::\n\n",
    );

    if !about.is_empty() {
        let _ = writeln!(out, "{about}\n");
    }
    if path.is_empty() {
        out.push_str(
            "Every invocation opens the workspace volumes, does one thing and exits. \
             See the [CLI usage](/docs/workspace/) guide for the workflow and \
             [Quick start](/docs/workspace/quickstart/) for a worked example.\n\n",
        );
    }

    out.push_str("## Usage\n\n```text\n");
    let usage = command.clone().bin_name(name).render_usage().to_string();
    let usage = usage
        .trim()
        .strip_prefix("Usage: ")
        .unwrap_or(usage.trim())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(out, "{usage}\n```\n");

    let table = arguments_table(command);
    if !table.is_empty() {
        out.push_str("## Arguments and options\n\n");
        out.push_str(&table);
        out.push('\n');
    }

    let subcommands: Vec<&Command> = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .collect();
    if !subcommands.is_empty() {
        out.push_str("## Commands\n\n| Command | Description |\n| --- | --- |\n");
        for sub in subcommands {
            let sub_name = sub.get_name();
            let link = if path.is_empty() {
                format!("/docs/workspace/cli/{sub_name}/")
            } else {
                format!("/docs/workspace/cli/{}/{sub_name}/", path.join("/"))
            };
            let help = sub.get_about().map(ToString::to_string).unwrap_or_default();
            let _ = writeln!(out, "| [`{sub_name}`]({link}) | {} |", cell(&help));
        }
        out.push('\n');
    }

    let file = if path.is_empty() {
        PathBuf::from("index.md")
    } else if has_subcommands {
        PathBuf::from(format!("{}/index.md", path.join("/")))
    } else {
        PathBuf::from(format!("{}.md", path.join("/")))
    };
    pages.push((file, out));

    for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        let sub_name = format!("{name} {}", sub.get_name());
        render_command(sub, &sub_name, pages);
    }
}

/// One table row per positional argument and option the command accepts.
fn arguments_table(command: &Command) -> String {
    let mut rows = String::new();
    for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
        let is_positional = arg.get_index().is_some();
        let required = arg.is_required_set();
        let value_name = arg
            .get_value_names()
            .and_then(|names| names.first())
            .map(ToString::to_string)
            .unwrap_or_default();

        let name = if is_positional {
            if required {
                format!("<{value_name}>")
            } else {
                format!("[{value_name}]")
            }
        } else {
            let mut name = String::new();
            if let Some(short) = arg.get_short() {
                let _ = write!(name, "-{short}, ");
            }
            if let Some(long) = arg.get_long() {
                let _ = write!(name, "--{long}");
            }
            if !value_name.is_empty() && arg.get_action().takes_values() {
                let _ = write!(name, " <{value_name}>");
            }
            name
        };

        let default = arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let possible = if arg.get_action().takes_values() {
            arg.get_possible_values()
                .iter()
                .filter(|value| !value.is_hide_set())
                .map(|value| format!("`{}`", value.get_name()))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            String::new()
        };
        let mut help = arg
            .get_long_help()
            .or_else(|| arg.get_help())
            .map(ToString::to_string)
            .unwrap_or_default();
        if !possible.is_empty() {
            if !help.is_empty() && !help.ends_with('.') {
                help.push('.');
            }
            let _ = write!(help, " One of: {possible}.");
        }

        let _ = writeln!(
            rows,
            "| {} | {} | {} |",
            cell(&name),
            cell(&default),
            cell(&help)
        );
    }

    if rows.is_empty() {
        return String::new();
    }
    format!("| Argument or option | Default | Description |\n| --- | --- | --- |\n{rows}")
}

/// Quote a value for YAML frontmatter.
fn yaml(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Collapse a help string into a single table cell.
fn cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

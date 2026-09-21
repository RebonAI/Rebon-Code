use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context};

pub fn validate_identifier(kind: &str, value: &str) -> anyhow::Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{kind} must not be empty");
    }
    if trimmed.len() > 128 {
        bail!("{kind} `{trimmed}` is too long");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("{kind} `{trimmed}` may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(())
}

pub fn validate_relative_asset_path(raw: &str) -> anyhow::Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("asset path must not be empty");
    }
    if trimmed.contains('\0') {
        bail!("asset path `{trimmed}` contains a NUL byte");
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        bail!("asset path `{trimmed}` must be relative to the plugin root");
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                bail!("asset path `{trimmed}` escapes the plugin root")
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(path)
}

pub fn ensure_path_inside_root(root: &Path, candidate: &Path) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|err| anyhow!("failed to resolve plugin root {}: {err}", root.display()))?;
    let candidate = candidate.canonicalize().map_err(|err| {
        anyhow!(
            "failed to resolve plugin asset path {}: {err}",
            candidate.display()
        )
    })?;
    if !candidate.starts_with(&root) {
        bail!(
            "plugin asset {} escapes plugin root {}",
            candidate.display(),
            root.display()
        );
    }
    Ok(())
}

/// Prefix of the placeholder that names a binary shipped beside `rebon`.
///
/// `{rebon_bin:rebon-browser-mcp}` — see [`expand_placeholders`].
const REBON_BIN_PREFIX: &str = "rebon_bin:";

/// Which sibling binary a `{rebon_bin:…}` placeholder names, if it is one.
///
/// Only `rebon-` prefixed names are accepted. The placeholder resolves to an
/// absolute path in a product location, so a manifest that could name anything
/// else would be a way to point a plugin's command at an arbitrary file next
/// to the executable.
fn rebon_bin_name(placeholder: &str) -> Option<Result<&str, String>> {
    let name = placeholder.strip_prefix(REBON_BIN_PREFIX)?;
    if name.starts_with("rebon-") && name.len() > "rebon-".len() {
        Some(Ok(name))
    } else {
        Some(Err(format!(
            "`{{{REBON_BIN_PREFIX}{name}}}` must name a binary that ships beside rebon, \
             whose name starts with `rebon-`"
        )))
    }
}

pub fn validate_placeholders(value: &str) -> anyhow::Result<()> {
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('}') else {
            bail!("unclosed placeholder in `{value}`");
        };
        let placeholder = &after_start[..end];
        match rebon_bin_name(placeholder) {
            Some(Ok(_)) => {}
            Some(Err(message)) => bail!("{message} (in `{value}`)"),
            None => {
                if placeholder != "plugin_dir" && placeholder != "rebon" {
                    bail!("unsupported placeholder `{{{placeholder}}}` in `{value}`");
                }
            }
        }
        rest = &after_start[end + 1..];
    }
    if value.contains('}') && !value.contains('{') {
        bail!("unmatched closing placeholder brace in `{value}`");
    }
    Ok(())
}

/// Substitutes the manifest placeholders against an installed plugin.
///
/// `{plugin_dir}` and `{rebon}` are textual. `{rebon_bin:<name>}` is not: it
/// resolves through [`rebon_types::sibling_binary`] against the product
/// locations beside `rebon`, and **fails** when the binary is not installed.
/// Writing an unresolvable command instead would produce an MCP server entry
/// that only fails when something tries to spawn it, at which point the
/// message is about a missing file rather than a missing part of the product.
pub fn expand_placeholders(value: &str, plugin_dir: &Path, rebon: &Path) -> anyhow::Result<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('}') else {
            // Left verbatim: `validate_placeholders` is what rejects a
            // malformed placeholder, and this function is not a second
            // validator with its own opinion.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let placeholder = &after_start[..end];
        match rebon_bin_name(placeholder) {
            Some(Ok(name)) => {
                let path = rebon_types::sibling_binary::resolve_from_executable(name, rebon)
                    .with_context(|| {
                        format!("failed to resolve `{{{REBON_BIN_PREFIX}{name}}}` in `{value}`")
                    })?;
                out.push_str(&path.to_string_lossy());
            }
            Some(Err(message)) => bail!("{message} (in `{value}`)"),
            None => match placeholder {
                "plugin_dir" => out.push_str(&plugin_dir.to_string_lossy()),
                "rebon" => out.push_str(&rebon.to_string_lossy()),
                other => {
                    // Same reason as the unclosed case above.
                    out.push('{');
                    out.push_str(other);
                    out.push('}');
                }
            },
        }
        rest = &after_start[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn reject_dangerous_shell_pipeline(value: &str) -> anyhow::Result<()> {
    let normalized = value.to_ascii_lowercase().replace(['\n', '\r', '\t'], " ");
    if normalized.contains("curl") && normalized.contains('|') && normalized.contains("sh") {
        bail!("commands such as `curl | sh` are not allowed in plugin manifests");
    }
    if normalized.contains("wget") && normalized.contains('|') && normalized.contains("sh") {
        bail!("commands such as `wget | sh` are not allowed in plugin manifests");
    }
    Ok(())
}

pub fn validate_stdio_command(
    command: &str,
    external_requirements: &[String],
) -> anyhow::Result<()> {
    validate_placeholders(command)?;
    reject_dangerous_shell_pipeline(command)?;
    if command == "{rebon}" || command.starts_with("{plugin_dir}") {
        return Ok(());
    }
    if command.contains('{') {
        return Ok(());
    }
    if command
        .chars()
        .any(|c| matches!(c, '|' | '&' | ';' | '>' | '<' | '`' | '$'))
    {
        bail!("MCP command `{command}` looks like a shell command; use command plus args instead");
    }
    if command.split_whitespace().count() > 1 {
        bail!("MCP command `{command}` must not include arguments; put arguments in `args`");
    }
    if Path::new(command).components().count() > 1 {
        return Ok(());
    }
    if external_requirements
        .iter()
        .any(|allowed| allowed == command)
    {
        return Ok(());
    }
    bail!(
        "external MCP command `{command}` must be declared in requirements.externalCommands or use `{{plugin_dir}}`/`{{rebon}}`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_relative_asset_path("../x").is_err());
        assert!(validate_relative_asset_path("skills/demo").is_ok());
    }

    #[test]
    fn allows_only_known_placeholders() {
        assert!(validate_placeholders("{plugin_dir}/bin/tool").is_ok());
        assert!(validate_placeholders("{home}/tool").is_err());
    }

    #[test]
    fn a_sibling_binary_placeholder_must_name_a_rebon_binary() {
        assert!(validate_placeholders("{rebon_bin:rebon-browser-mcp}").is_ok());
        let error = validate_placeholders("{rebon_bin:sh}")
            .expect_err("only `rebon-` prefixed binaries may be named")
            .to_string();
        assert!(error.contains("rebon-"), "{error}");
        assert!(validate_placeholders("{rebon_bin:rebon-}").is_err());
    }

    #[test]
    fn a_sibling_binary_placeholder_expands_to_the_installed_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let rebon = directory
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon"));
        let sibling = directory
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon-browser-mcp"));
        std::fs::write(&sibling, b"binary").expect("write sibling");
        let expanded =
            expand_placeholders("{rebon_bin:rebon-browser-mcp}", directory.path(), &rebon)
                .expect("the sibling is installed");
        assert_eq!(PathBuf::from(expanded), sibling);
    }

    #[test]
    fn a_missing_sibling_binary_fails_materialization() {
        let directory = tempfile::tempdir().expect("temp dir");
        let rebon = directory
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon"));
        let error = expand_placeholders("{rebon_bin:rebon-browser-mcp}", directory.path(), &rebon)
            .expect_err("nothing was installed beside the executable")
            .to_string();
        assert!(error.contains("rebon-browser-mcp"), "{error}");
    }

    #[test]
    fn the_textual_placeholders_still_expand_verbatim() {
        let expanded = expand_placeholders(
            "{plugin_dir}/extension and {rebon}",
            Path::new("/plugins/browser"),
            Path::new("/bin/rebon"),
        )
        .expect("no sibling binary is named");
        assert_eq!(expanded, "/plugins/browser/extension and /bin/rebon");
    }

    #[test]
    fn rejects_shell_style_stdio_commands() {
        assert!(validate_stdio_command("node server.js", &[]).is_err());
        assert!(validate_stdio_command("curl https://example.invalid | sh", &[]).is_err());
        assert!(validate_stdio_command("node", &["node".to_string()]).is_ok());
    }
}

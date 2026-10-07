//! Reading a Claude Code mod folder as a package rebon can load.
//!
//! A mod folder carries no `rebon-plugin.json`. Its ceiling — what the hooks
//! module may register and reach — is read off the module's own source
//! instead, the way `claude plugin validate` reads it: the `on(...)`
//! patterns, the `$.noun.method` calls, and the names spelled as literals in
//! `$.command.register({ name })` and `$.tool.register({ name })`. From that
//! scan this module synthesises the [`KernelPluginManifest`] the plane loads
//! the mod against, the [`PluginManifest`] an install records, and the marker
//! the `mods-runtime` loader reads off the load request.
//!
//! The scanner is deliberately literal. A name computed at run time is not
//! in the ceiling, and `$.command.register` refuses it with a line that says
//! to spell it as a literal — a ceiling a person can read before loading is
//! the whole point of the plane.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rebon_types::{
    ClaudeModHooksFile, ClaudeModManifest, KernelPluginManifest, KernelPluginRoot,
    KernelPluginSettingKey, ModCommandDecl, ModScan, ModToolDecl, ALL_REBON_TOOLS,
    CLAUDE_HOOKS_FILE, CLAUDE_PLUGIN_DIRS_ENV, CLAUDE_PLUGIN_MANIFEST, MODS_DIR, MODS_SEAT,
    MOD_DIRS_ENV, MOD_MARKER, MOD_SERVICE,
};
use serde_json::Value;

use crate::compatibility::{CompatibilityDeclaration, PluginFormat};
use crate::manifest::{PluginCapabilities, PluginManifest};

/// The seats every mod may call: its own `$`, its settings namespace, and
/// the plane's logger.
pub const MOD_SEATS: &[&str] = &[MODS_SEAT, "settings", "logger"];

/// A mod folder, read.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeMod {
    /// The folder holding `.claude-plugin/plugin.json`, as given.
    pub root: PathBuf,
    pub manifest: ClaudeModManifest,
    pub compatibility: Option<CompatibilityDeclaration>,
    pub hooks: ClaudeModHooksFile,
    /// The hooks module, relative to the root with forward slashes.
    pub hooks_module: String,
    pub scan: ModScan,
}

impl ClaudeMod {
    pub fn declare_install_compatibility(&mut self) {
        if self.compatibility.is_none() {
            self.compatibility = Some(CompatibilityDeclaration {
                format: PluginFormat::ClaudeMods.name().to_owned(),
                format_version: crate::compatibility::FORMAT_VERSION,
                adapter_revision: crate::compatibility::ADAPTER_REVISION,
                sdk: vec![crate::compatibility::SdkRequirement {
                    name: "rebon-claude-mods-api".to_owned(),
                    range: "^1".to_owned(),
                }],
                dsh_snapshot: None,
            });
        }
    }
}

/// Whether `path` is a folder with a mod manifest in it.
pub fn is_claude_mod_dir(path: &Path) -> bool {
    path.join(CLAUDE_PLUGIN_MANIFEST).is_file()
}

/// Reads the folder: manifest, hooks file, and the hooks module's scan.
pub fn read_claude_mod(root: &Path) -> Result<ClaudeMod, String> {
    let manifest_path = root.join(CLAUDE_PLUGIN_MANIFEST);
    let raw = std::fs::read(&manifest_path)
        .map_err(|error| format!("{} is unreadable: {error}", manifest_path.display()))?;
    let manifest: ClaudeModManifest = serde_json::from_slice(&raw).map_err(|error| {
        format!(
            "{} is not a plugin manifest: {error}",
            manifest_path.display()
        )
    })?;
    if manifest.name.trim().is_empty() {
        return Err(format!("{} names no plugin", manifest_path.display()));
    }
    if !manifest
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "{} names the plugin {:?}, which is not a name made of letters, digits, '-', '_' and '.'",
            manifest_path.display(),
            manifest.name
        ));
    }
    let compatibility = manifest
        .rest
        .get("rebon")
        .map(|value| {
            CompatibilityDeclaration::read(&manifest.name, Some(value), PluginFormat::ClaudeMods)
        })
        .transpose()
        .map_err(|error| error.to_string())?;
    let hooks_path = root.join(CLAUDE_HOOKS_FILE);
    let raw = std::fs::read(&hooks_path).map_err(|error| {
        format!(
            "{} has no {CLAUDE_HOOKS_FILE} naming its hooks module: {error}",
            root.display()
        )
    })?;
    let hooks: ClaudeModHooksFile = serde_json::from_slice(&raw)
        .map_err(|error| format!("{} is not a hooks file: {error}", hooks_path.display()))?;
    let module = match hooks.modules.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(format!(
                "{} names no hooks module under `modules`",
                hooks_path.display()
            ))
        }
        many => {
            return Err(format!(
                "{} names {} hooks modules; a mod has exactly one",
                hooks_path.display(),
                many.len()
            ))
        }
    };
    let hooks_module = module_relative_to_root(&module)?;
    let module_path = root.join(&hooks_module);
    let source = std::fs::read_to_string(&module_path).map_err(|error| {
        format!(
            "hooks module {} is unreadable: {error}",
            module_path.display()
        )
    })?;
    let scan = scan_hooks_module(&source, &manifest.name);
    Ok(ClaudeMod {
        root: root.to_path_buf(),
        manifest,
        compatibility,
        hooks,
        hooks_module,
        scan,
    })
}

/// `hooks.json` names the module relative to itself; the plane wants it
/// relative to the package root, with forward slashes and no escape.
fn module_relative_to_root(module: &str) -> Result<String, String> {
    let trimmed = module.trim().trim_start_matches("./").replace('\\', "/");
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.contains(':') {
        return Err(format!(
            "hooks module {module:?} is not a path relative to hooks/"
        ));
    }
    if trimmed.split('/').any(|part| part == "..") {
        return Err(format!("hooks module {module:?} leaves the mod's folder"));
    }
    let extension = Path::new(&trimmed)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !matches!(
        extension,
        "ts" | "tsx" | "jsx" | "js" | "mjs" | "cjs" | "mts" | "cts"
    ) {
        return Err(format!(
            "hooks module {module:?} is not named .ts, .tsx, .jsx, .js, .mjs, .cjs, .mts or .cts"
        ));
    }
    Ok(format!("hooks/{trimmed}"))
}

// ---- the scan ---------------------------------------------------------

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// The source with its comments and the insides of its strings blanked,
/// so a pattern in a comment or a `$.` inside a string is not read as a
/// registration. Positions are preserved, which is what lets the literal
/// extractors read the original text back at the same offsets.
fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            out.push_str("  ");
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.push_str("  ");
            i += 2;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            out.push(c);
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    out.push(' ');
                    i += 1;
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
                if c != '`' && chars[i] == '\n' {
                    break;
                }
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i < chars.len() {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The string literal starting at `at` in `source`, when one does.
fn string_literal_at(source: &[char], at: usize) -> Option<String> {
    let quote = *source.get(at)?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    let mut out = String::new();
    let mut i = at + 1;
    while i < source.len() {
        let c = source[i];
        if c == '\\' {
            if let Some(escaped) = source.get(i + 1) {
                out.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    other => *other,
                });
            }
            i += 2;
            continue;
        }
        if c == quote {
            return Some(out);
        }
        if c == '$' && quote == '`' && source.get(i + 1) == Some(&'{') {
            // A substitution: not a literal the ceiling can read.
            return None;
        }
        out.push(c);
        i += 1;
    }
    None
}

fn skip_space(source: &[char], mut i: usize) -> usize {
    while i < source.len() && source[i].is_whitespace() {
        i += 1;
    }
    i
}

/// The index of the `}` matching the `{` at `open`, on code with strings
/// blanked.
fn matching_brace(code: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in code[open..].iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// `key: "literal"` inside an object literal spanning `[open, close]`, or
/// `key: NAME` where the module spells `const NAME = "literal"`.
fn literal_prop(
    code: &[char],
    source: &[char],
    open: usize,
    close: usize,
    key: &str,
) -> Option<String> {
    let key_chars: Vec<char> = key.chars().collect();
    let mut i = open + 1;
    while i + key_chars.len() < close {
        let before_ok = !is_ident(code[i - 1]);
        if before_ok && code[i..i + key_chars.len()] == key_chars[..] {
            let mut j = skip_space(code, i + key_chars.len());
            if code.get(j) == Some(&':') {
                j = skip_space(code, j + 1);
                if let Some(value) = string_literal_at(source, j) {
                    return Some(value);
                }
                let ident: String = code[j..].iter().take_while(|c| is_ident(**c)).collect();
                return const_string(code, source, &ident);
            }
        }
        i += 1;
    }
    None
}

/// The literal a `const <ident> = "literal"` in the module binds, when one
/// does: a name kept in one constant is still a name spelled in the source.
/// Only a `const`, and only the first one, so what is scanned is what runs.
fn const_string(code: &[char], source: &[char], ident: &str) -> Option<String> {
    if ident.is_empty() || ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let needle: Vec<char> = "const".chars().collect();
    let ident: Vec<char> = ident.chars().collect();
    let mut i = 0;
    while i + needle.len() < code.len() {
        let starts_word = i == 0 || !is_ident(code[i - 1]);
        if starts_word && code[i..i + needle.len()] == needle[..] {
            let at = i + needle.len();
            let j = skip_space(code, at);
            let named = j > at
                && code.get(j..j + ident.len()) == Some(&ident[..])
                && !code.get(j + ident.len()).copied().is_some_and(is_ident);
            if named {
                let k = skip_space(code, j + ident.len());
                if code.get(k) == Some(&'=') && code.get(k + 1) != Some(&'=') {
                    return string_literal_at(source, skip_space(code, k + 1));
                }
            }
        }
        i += 1;
    }
    None
}

/// Every `$.noun.method(` with an object literal as its first argument:
/// `(name, description, argumentHint)` read off the literal.
fn registrations(
    code: &[char],
    source: &[char],
    call: &str,
    warnings: &mut Vec<String>,
) -> Vec<(String, Option<String>, Option<String>)> {
    let needle: Vec<char> = call.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= code.len() {
        if code[i..i + needle.len()] != needle[..] || (i > 0 && is_ident(code[i - 1])) {
            i += 1;
            continue;
        }
        let mut j = skip_space(code, i + needle.len());
        if code.get(j) != Some(&'(') {
            i += 1;
            continue;
        }
        j = skip_space(code, j + 1);
        if code.get(j) != Some(&'{') {
            warnings.push(format!(
                "a {call}() call does not pass an object literal, so its name is not in the ceiling"
            ));
            i = j;
            continue;
        }
        let Some(close) = matching_brace(code, j) else {
            i = j;
            continue;
        };
        match literal_prop(code, source, j, close, "name") {
            Some(name) if !name.trim().is_empty() => out.push((
                name,
                literal_prop(code, source, j, close, "description"),
                literal_prop(code, source, j, close, "argumentHint"),
            )),
            _ => warnings.push(format!(
                "a {call}() call does not spell its name as a string literal, so it is not in the ceiling"
            )),
        }
        i = close;
    }
    out
}

/// Reads what a hooks module registers and calls.
///
/// `plugin` is the mod's name, which a registered tool's model-facing name
/// carries (`mcp__<plugin>__<name>`).
pub fn scan_hooks_module(source: &str, plugin: &str) -> ModScan {
    let code_string = code_only(source);
    let code: Vec<char> = code_string.chars().collect();
    let source_chars: Vec<char> = source.chars().collect();
    let mut scan = ModScan::default();

    // on("pattern", ...)
    let mut i = 0;
    while i + 2 < code.len() {
        if code[i] == 'o'
            && code[i + 1] == 'n'
            && (i == 0 || !is_ident(code[i - 1]) && code[i - 1] != '.')
        {
            let j = skip_space(&code, i + 2);
            if code.get(j) == Some(&'(') {
                let k = skip_space(&code, j + 1);
                match string_literal_at(&source_chars, k) {
                    Some(pattern) if !scan.events.contains(&pattern) => scan.events.push(pattern),
                    Some(_) => {}
                    None => scan.warnings.push(
                        "an on() call does not spell its event as a string literal".to_owned(),
                    ),
                }
            }
        }
        i += 1;
    }

    // $.noun.method
    let mut calls = Vec::new();
    let mut i = 0;
    while i + 1 < code.len() {
        if code[i] == '$' && code[i + 1] == '.' && (i == 0 || !is_ident(code[i - 1])) {
            let mut j = i + 2;
            let start = j;
            while j < code.len() && is_ident(code[j]) {
                j += 1;
            }
            let noun: String = code[start..j].iter().collect();
            if !noun.is_empty() && code.get(j) == Some(&'.') {
                let mut k = j + 1;
                let m = k;
                while k < code.len() && is_ident(code[k]) {
                    k += 1;
                }
                let method: String = code[m..k].iter().collect();
                if !method.is_empty() {
                    let call = format!("{noun}.{method}");
                    if !calls.contains(&call) {
                        calls.push(call);
                    }
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    calls.sort();
    scan.calls = calls;

    for (name, description, argument_hint) in registrations(
        &code,
        &source_chars,
        "$.command.register",
        &mut scan.warnings,
    ) {
        if scan.commands.iter().any(|c| c.name == name) {
            continue;
        }
        scan.commands.push(ModCommandDecl {
            name,
            description,
            argument_hint,
        });
    }
    for (name, description, _) in
        registrations(&code, &source_chars, "$.tool.register", &mut scan.warnings)
    {
        let full = if name.starts_with("mcp__") {
            name
        } else {
            format!("mcp__{plugin}__{name}")
        };
        if scan.tools.iter().any(|t| t.name == full) {
            continue;
        }
        scan.tools.push(ModToolDecl {
            name: full,
            description,
        });
    }

    for (call, into) in [
        ("$.env.get", &mut scan.env_reads),
        ("$.env.set", &mut scan.env_writes),
    ] {
        let needle: Vec<char> = call.chars().collect();
        let mut i = 0;
        while i + needle.len() <= code.len() {
            if code[i..i + needle.len()] == needle[..] && (i == 0 || !is_ident(code[i - 1])) {
                let j = skip_space(&code, i + needle.len());
                if code.get(j) == Some(&'(') {
                    let k = skip_space(&code, j + 1);
                    if let Some(name) = string_literal_at(&source_chars, k) {
                        if !into.contains(&name) {
                            into.push(name);
                        }
                    }
                }
                i += needle.len();
                continue;
            }
            i += 1;
        }
        into.sort();
    }
    scan
}

// ---- what the scan becomes ---------------------------------------------

/// The settings keys a mod's `userConfig` declares, in the plane's shape.
fn setting_keys(manifest: &ClaudeModManifest) -> Vec<KernelPluginSettingKey> {
    manifest
        .user_config
        .iter()
        .map(|(name, field)| KernelPluginSettingKey {
            name: name.clone(),
            ty: field.ty.clone(),
            default: field.default.clone(),
        })
        .collect()
}

/// The ceiling the plane loads the mod against.
pub fn kernel_manifest_for(mod_: &ClaudeMod) -> KernelPluginManifest {
    let reaches_tools = mod_.scan.calls("tool.call") || mod_.scan.calls("mcp.call");
    KernelPluginManifest {
        root: KernelPluginRoot::Package,
        entry: Some(mod_.hooks_module.clone()),
        services: vec![MOD_SERVICE.to_owned()],
        event_topics: Vec::new(),
        published_topics: Vec::new(),
        llm_providers: Vec::new(),
        tools: mod_.scan.tools.iter().map(|t| t.name.clone()).collect(),
        commands: mod_.scan.commands.iter().map(|c| c.name.clone()).collect(),
        invokable_tools: if reaches_tools {
            Value::String(ALL_REBON_TOOLS.to_owned())
        } else {
            Value::Array(Vec::new())
        },
        seats: MOD_SEATS.iter().map(|s| (*s).to_owned()).collect(),
        settings: setting_keys(&mod_.manifest),
    }
}

/// The package manifest an install records for the folder.
pub fn plugin_manifest_for(mod_: &ClaudeMod) -> PluginManifest {
    let mut kernel_plugins = BTreeMap::new();
    kernel_plugins.insert(mod_.manifest.name.clone(), kernel_manifest_for(mod_));
    let mut metadata = BTreeMap::new();
    metadata.insert("claudeMod".to_owned(), Value::Bool(true));
    metadata.insert(
        "hooksModule".to_owned(),
        Value::String(mod_.hooks_module.clone()),
    );
    PluginManifest {
        name: mod_.manifest.name.clone(),
        version: mod_
            .manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_owned()),
        compatibility: mod_.compatibility.clone(),
        description: mod_.manifest.description.clone(),
        source: None,
        capabilities: PluginCapabilities {
            kernel_plugins,
            ..PluginCapabilities::default()
        },
        requirements: Default::default(),
        integrity: None,
        metadata,
        container: None,
    }
}

/// `options`, as `register(on, options)` receives them: the manifest's
/// defaults, each overridden by a stored value of the field's own kind.
///
/// A `string` field that lists `options` is a picker over exactly those
/// values, and a stored value outside them counts as unset.
pub fn user_config_options(manifest: &ClaudeModManifest, stored: Option<&Value>) -> Value {
    let stored = stored.and_then(Value::as_object);
    let mut out = serde_json::Map::new();
    for (name, field) in &manifest.user_config {
        let ty = field.ty.as_deref().unwrap_or("string");
        let given = stored.and_then(|map| map.get(name)).filter(|value| {
            let right_kind = match ty {
                "boolean" => value.is_boolean(),
                "number" => value.is_number(),
                _ => value.is_string(),
            };
            let listed = field.options.is_empty()
                || value
                    .as_str()
                    .is_some_and(|s| field.options.iter().any(|o| o == s));
            right_kind && listed
        });
        if let Some(value) = given.or(field.default.as_ref()) {
            out.insert(name.clone(), value.clone());
        }
    }
    Value::Object(out)
}

/// The marker rebon puts on the mod's load request, under
/// [`MOD_MARKER`] in `config`: everything the loader needs to know without
/// reading the module twice.
pub fn mod_marker(mod_: &ClaudeMod, options: Value) -> Value {
    serde_json::json!({
        "name": mod_.manifest.name,
        "version": mod_.manifest.version,
        "options": options,
        "commands": mod_.scan.commands,
        "tools": mod_.scan.tools,
        "events": mod_.scan.events,
        "calls": mod_.scan.calls,
    })
}

/// The load request's `config` for a mod.
pub fn mod_config(mod_: &ClaudeMod, options: Value) -> Value {
    let mut config = serde_json::Map::new();
    config.insert(MOD_MARKER.to_owned(), mod_marker(mod_, options));
    Value::Object(config)
}

/// Whether a load request's config says the entry is a mod.
pub fn is_mod_config(config: &Value) -> bool {
    config.get(MOD_MARKER).is_some_and(Value::is_object)
}

/// The folders this machine loads mods from: every child of
/// `<config_home>/mods` holding a manifest, then the folders `REBON_MOD_DIRS`
/// and `CLAUDE_CODE_PLUGIN_DIRS` list, path-separator joined, `~` allowed.
///
/// `env` is injected so a test can say what the variables hold.
pub fn discover_mod_dirs(
    config_home: &Path,
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if is_claude_mod_dir(&dir) && !out.contains(&dir) {
            out.push(dir);
        }
    };
    let mods = config_home.join(MODS_DIR);
    if let Ok(entries) = std::fs::read_dir(&mods) {
        let mut children: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        for child in children {
            push(child);
        }
    }
    for variable in [MOD_DIRS_ENV, CLAUDE_PLUGIN_DIRS_ENV] {
        let Some(list) = env(variable) else { continue };
        for raw in std::env::split_paths(&list) {
            let text = raw.to_string_lossy();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let expanded = match (
                trimmed
                    .strip_prefix("~/")
                    .or_else(|| trimmed.strip_prefix("~\\")),
                home,
            ) {
                (Some(rest), Some(home)) => home.join(rest),
                _ if trimmed == "~" => match home {
                    Some(home) => home.to_path_buf(),
                    None => continue,
                },
                _ => PathBuf::from(trimmed),
            };
            push(expanded);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MODULE: &str = r#"
import type { Register } from 'claude-code';
import { update } from 'claude-code';
// on("never", () => {}) in a comment
export const register: Register = (on, options) => {
  on('session.start', async ($, e, next) => {
    await $.command.register({ name: 'tally', description: 'Counts things', argumentHint: '[n]' });
    await $.tool.register({
      name: "count",
      description: "Counts",
      inputSchema: { type: "object", properties: { by: { type: "number" } } },
    });
    $.ui.status(`tally ${options.prefix}`);
    return next(e);
  });
  on("tool.call", { tool: "Bash" }, async ($, e, next) => next(e));
  on('tool.call', { tool: 'mcp__tally__count' }, async () => ({ result: 1 }));
  on(`classic.Stop`, async ($, e) => ({ systemMessage: "$.fake.call in a string" }));
  on('ui.render', { component: 'Pane' }, async ($, e) => { const { Box } = $.ui.resolve(e); return <Box/>; });
  const name = 'computed';
  on('command.run', { command: name }, async ($) => { $.command.register({ name }); return { text: $.env.get('HOME') }; });
};
"#;

    #[test]
    fn a_name_kept_in_a_const_is_read_as_spelled() {
        let scan = scan_hooks_module(
            r#"
const NAME = 'mod-template'
const HINT = "[n]" as const
let LOOSE = 'not-a-const'
export const register = on => {
  on('session.start', async ($, e, next) => {
    await $.command.register({ name: NAME, description: 'Says the turns', argumentHint: HINT })
    await $.command.register({ name: LOOSE })
    await $.command.register({ name: MISSING })
    return next(e)
  })
}
"#,
            "template",
        );
        assert_eq!(
            scan.commands
                .iter()
                .map(|c| (c.name.as_str(), c.argument_hint.as_deref()))
                .collect::<Vec<_>>(),
            vec![("mod-template", Some("[n]"))]
        );
        assert_eq!(
            scan.warnings
                .iter()
                .filter(|w| w.contains("does not spell its name"))
                .count(),
            2,
            "a `let` and an unknown name are still not spelled: {:?}",
            scan.warnings
        );
    }

    #[test]
    fn const_string_takes_only_a_const_bound_to_a_literal() {
        let source: Vec<char> = "const A = 'a'; const AB = 'ab'; const C = make(); const D == 'x'"
            .chars()
            .collect();
        let code = source.clone();
        assert_eq!(const_string(&code, &source, "A").as_deref(), Some("a"));
        assert_eq!(const_string(&code, &source, "AB").as_deref(), Some("ab"));
        assert_eq!(const_string(&code, &source, "C"), None);
        assert_eq!(const_string(&code, &source, "D"), None);
        assert_eq!(const_string(&code, &source, ""), None);
        assert_eq!(const_string(&code, &source, "Z"), None);
    }

    #[test]
    fn the_scan_reads_patterns_calls_and_literal_registrations() {
        let scan = scan_hooks_module(MODULE, "tally");
        assert_eq!(
            scan.events,
            vec![
                "session.start",
                "tool.call",
                "classic.Stop",
                "ui.render",
                "command.run"
            ]
        );
        assert_eq!(
            scan.calls,
            vec![
                "command.register",
                "env.get",
                "tool.register",
                "ui.resolve",
                "ui.status"
            ]
        );
        assert_eq!(scan.commands.len(), 1);
        assert_eq!(scan.commands[0].name, "tally");
        assert_eq!(
            scan.commands[0].description.as_deref(),
            Some("Counts things")
        );
        assert_eq!(scan.commands[0].argument_hint.as_deref(), Some("[n]"));
        assert_eq!(scan.tools.len(), 1);
        assert_eq!(scan.tools[0].name, "mcp__tally__count");
        assert_eq!(scan.tools[0].description.as_deref(), Some("Counts"));
        assert_eq!(scan.env_reads, vec!["HOME"]);
        assert!(scan.env_writes.is_empty());
        assert_eq!(scan.warnings.len(), 1, "{:?}", scan.warnings);
        assert!(scan.warnings[0].contains("$.command.register"));
    }

    #[test]
    fn strings_and_comments_hide_nothing_that_is_not_code() {
        let scan = scan_hooks_module(
            "/* $.a.b */ const s = \"$.c.d\"; on('x', () => 1); // on('y')",
            "m",
        );
        assert_eq!(scan.events, vec!["x"]);
        assert!(scan.calls.is_empty());
    }

    fn write_mod(root: &Path, module: &str) {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join(CLAUDE_PLUGIN_MANIFEST),
            json!({
                "name": "tally",
                "version": "1.2.3",
                "description": "Counts",
                "userConfig": {
                    "prefix": { "type": "string", "default": "n=", "options": ["n=", "#"] },
                    "loud": { "type": "boolean", "default": false }
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(CLAUDE_HOOKS_FILE),
            r#"{ "modules": ["./register.tsx"] }"#,
        )
        .unwrap();
        std::fs::write(root.join("hooks/register.tsx"), module).unwrap();
    }

    #[test]
    fn a_folder_reads_as_a_mod_and_synthesises_its_manifests() {
        let dir = tempfile::tempdir().unwrap();
        write_mod(dir.path(), MODULE);
        assert!(is_claude_mod_dir(dir.path()));
        let mod_ = read_claude_mod(dir.path()).unwrap();
        assert_eq!(mod_.hooks_module, "hooks/register.tsx");
        let kernel = kernel_manifest_for(&mod_);
        assert_eq!(kernel.entry.as_deref(), Some("hooks/register.tsx"));
        assert_eq!(kernel.services, vec!["mod"]);
        assert_eq!(kernel.commands, vec!["tally"]);
        assert_eq!(kernel.tools, vec!["mcp__tally__count"]);
        assert_eq!(kernel.seats, vec!["mods", "settings", "logger"]);
        assert_eq!(kernel.settings.len(), 2);
        // The module calls no `$.tool.call`, so it reaches no tool.
        assert_eq!(kernel.invokable(&["Read".to_owned()]), Vec::<String>::new());
        kernel.validate_for_package("tally").unwrap();

        let package = plugin_manifest_for(&mod_);
        assert_eq!(package.name, "tally");
        assert_eq!(package.version, "1.2.3");
        assert!(package.capabilities.kernel_plugins.contains_key("tally"));
        assert_eq!(package.metadata["claudeMod"], json!(true));
    }

    #[test]
    fn only_installation_supplies_a_missing_mod_contract_and_preserves_overrides() {
        for explicit in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            write_mod(dir.path(), MODULE);
            let path = dir.path().join(CLAUDE_PLUGIN_MANIFEST);
            if explicit {
                let mut raw: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                raw["rebon"] = json!({"format":"claude-mods","formatVersion":1,"adapterRevision":1,"sdk":[{"name":"rebon-claude-mods-api","range":"=1.0.0"}]});
                std::fs::write(&path, raw.to_string()).unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            let mut mod_ = read_claude_mod(dir.path()).unwrap();
            assert_eq!(mod_.compatibility.is_some(), explicit);
            mod_.declare_install_compatibility();
            let declared = mod_.compatibility.clone();
            mod_.declare_install_compatibility();
            assert_eq!(mod_.compatibility, declared);
            let manifest = plugin_manifest_for(&mod_);
            let declaration = manifest.compatibility.as_ref().unwrap();
            assert_eq!(declaration.format, "claude-mods");
            assert_eq!(
                declaration.sdk[0].range,
                if explicit { "=1.0.0" } else { "^1" }
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                read_claude_mod(dir.path()).unwrap().compatibility.is_some(),
                explicit
            );
        }
    }

    #[test]
    fn a_mod_that_calls_tools_reaches_every_tool() {
        let dir = tempfile::tempdir().unwrap();
        write_mod(dir.path(), "export const register = (on) => on('x', async ($) => $.tool.call({ tool: 'Read', input: {} }));");
        let mod_ = read_claude_mod(dir.path()).unwrap();
        let kernel = kernel_manifest_for(&mod_);
        assert_eq!(
            kernel.invokable(&["Read".to_owned(), "Bash".to_owned()]),
            vec!["Read", "Bash"]
        );
    }

    #[test]
    fn options_take_the_defaults_and_stored_values_of_the_right_kind() {
        let dir = tempfile::tempdir().unwrap();
        write_mod(dir.path(), MODULE);
        let mod_ = read_claude_mod(dir.path()).unwrap();
        assert_eq!(
            user_config_options(&mod_.manifest, None),
            json!({ "prefix": "n=", "loud": false })
        );
        assert_eq!(
            user_config_options(
                &mod_.manifest,
                Some(&json!({ "prefix": "#", "loud": "yes" }))
            ),
            json!({ "prefix": "#", "loud": false })
        );
        // A value outside the picker counts as unset.
        assert_eq!(
            user_config_options(&mod_.manifest, Some(&json!({ "prefix": "zzz" }))),
            json!({ "prefix": "n=", "loud": false })
        );
        let config = mod_config(&mod_, json!({ "prefix": "#" }));
        assert!(is_mod_config(&config));
        assert_eq!(config[MOD_MARKER]["name"], json!("tally"));
        assert_eq!(config[MOD_MARKER]["commands"][0]["name"], json!("tally"));
        assert!(!is_mod_config(&json!({})));
    }

    #[test]
    fn a_folder_without_a_hooks_module_or_with_an_escaping_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.path().join(CLAUDE_PLUGIN_MANIFEST),
            r#"{ "name": "x" }"#,
        )
        .unwrap();
        assert!(read_claude_mod(dir.path())
            .unwrap_err()
            .contains("hooks/hooks.json"));
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join(CLAUDE_HOOKS_FILE),
            r#"{ "modules": ["../evil.ts"] }"#,
        )
        .unwrap();
        assert!(read_claude_mod(dir.path()).unwrap_err().contains("leaves"));
        std::fs::write(
            dir.path().join(CLAUDE_HOOKS_FILE),
            r#"{ "modules": ["./a.ts", "./b.ts"] }"#,
        )
        .unwrap();
        assert!(read_claude_mod(dir.path())
            .unwrap_err()
            .contains("exactly one"));
        std::fs::write(
            dir.path().join(CLAUDE_HOOKS_FILE),
            r#"{ "modules": ["./a.py"] }"#,
        )
        .unwrap();
        assert!(read_claude_mod(dir.path())
            .unwrap_err()
            .contains("not named"));
        std::fs::write(
            dir.path().join(CLAUDE_PLUGIN_MANIFEST),
            r#"{ "name": "bad name" }"#,
        )
        .unwrap();
        assert!(read_claude_mod(dir.path()).unwrap_err().contains("letters"));
    }

    #[test]
    fn mod_folders_are_found_under_the_config_home_and_the_two_variables() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config = home.join(".rebon");
        write_mod(
            &config.join("mods").join("b"),
            "export const register = () => {};",
        );
        write_mod(
            &config.join("mods").join("a"),
            "export const register = () => {};",
        );
        std::fs::create_dir_all(config.join("mods").join("not-a-mod")).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        write_mod(&elsewhere, "export const register = () => {};");
        write_mod(&home.join("tilde"), "export const register = () => {};");
        let joined = std::env::join_paths([elsewhere.clone(), dir.path().join("missing")]).unwrap();
        let env = |name: &str| match name {
            MOD_DIRS_ENV => Some(joined.to_string_lossy().into_owned()),
            CLAUDE_PLUGIN_DIRS_ENV => Some("~/tilde".to_owned()),
            _ => None,
        };
        let found = discover_mod_dirs(&config, Some(&home), env);
        assert_eq!(
            found,
            vec![
                config.join("mods").join("a"),
                config.join("mods").join("b"),
                elsewhere,
                home.join("tilde"),
            ]
        );
    }
}

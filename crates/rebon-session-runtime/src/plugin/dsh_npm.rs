//! A DeepSeek Harness package from npm, made into a package rebon installs.
//!
//! DSH publishes its plugins to npm as Cordis packages: a `package.json`, a
//! module exporting `apply`, and peer dependencies on the seams it expects
//! (`@deepseek-ai/dsh-tools`, `-web`, `-llm`, …). No rebon manifest, so
//! nothing says what it registers or reaches. This module writes one:
//!
//! 1. the package is slimmed to what runs — its own dependencies, plus the
//!    peers rebon does not answer itself (`provided-modules.json` lists the
//!    ones it does) — and npm installs that, with no scripts, no dev tree and
//!    no peer chains (those are DSH's own engine, which rebon replaces);
//! 2. the package is *probed*: mounted once against rebon's seats in a Node
//!    under the permission model (its own files readable, nothing writable,
//!    no processes) with a sink that records every registration. A package
//!    needing a service rebon does not offer, or an API rebon's seams do not
//!    have, fails here, with that named — before anything is installed;
//! 3. what it registered becomes the ceiling in `rebon-plugin.json`, and the
//!    hosts and variables its code names become its container request, both
//!    shown to the person at install.
//!
//! A DSH *skill* package is the other shape: its code is a skill provider
//! that hands DSH's engine the `SKILL.md` folders it ships. Rebon's own skill
//! loader reads those folders directly, so such a package becomes a rebon
//! package declaring them as skills — nothing of it runs, nothing is fetched,
//! and there is no container to grant.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};
use rebon_harness::rebon_plugin_package::compatibility::{
    host_sdk_versions, npm_sdk_requirements, CompatibilityDeclaration, PluginFormat,
    ADAPTER_REVISION, FORMAT_VERSION,
};
use rebon_harness::rebon_plugin_package::container::ContainerRequest;
use serde_json::{json, Value};

/// The seats every DSH package may call: credentials are gated by their own
/// grant, the logger and the plugin's own settings namespace are harmless.
const SEATS: &[&str] = &["credentials", "logger", "settings"];

/// Hosts a package names that are not hosts it connects to.
const NOT_ENDPOINTS: &[&str] = &[
    "github.com",
    "www.github.com",
    "npmjs.com",
    "www.npmjs.com",
    "json-schema.org",
    "example.com",
    "www.example.com",
    "localhost",
    "127.0.0.1",
];

/// Whether `dir` is an npm Cordis package rather than a rebon one.
pub fn is_cordis_npm_package(dir: &Path) -> bool {
    if dir.join("rebon-plugin.json").is_file() {
        return false;
    }
    let Ok(raw) = std::fs::read(dir.join("package.json")) else {
        return false;
    };
    let Ok(package) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let name = package.get("name").and_then(Value::as_str).unwrap_or("");
    let peers = package.get("peerDependencies").and_then(Value::as_object);
    name.starts_with("@deepseek-ai/")
        || peers.is_some_and(|peers| {
            peers.contains_key("@deepseek-ai/cordis") || peers.contains_key("cordis")
        })
}

/// The bare specifiers the composition answers itself.
pub fn provided_modules(compose_root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let path = compose_root.join("src").join("provided-modules.json");
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value = serde_json::from_slice(&raw)?;
    Ok(["payload", "runtime"]
        .iter()
        .filter_map(|section| doc.get(section).and_then(Value::as_object))
        .flat_map(|map| map.keys().cloned())
        .collect())
}

/// The package as rebon installs it: what runs, and the peers rebon lacks.
pub fn slim_package(package: &Value, provided: &BTreeSet<String>) -> Value {
    let mut dependencies = package
        .get("dependencies")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(peers) = package.get("peerDependencies").and_then(Value::as_object) {
        for (name, range) in peers {
            if !provided.contains(name) {
                dependencies.insert(name.clone(), range.clone());
            }
        }
    }
    let mut slim = serde_json::Map::new();
    for key in [
        "name",
        "version",
        "type",
        "main",
        "exports",
        "module",
        "description",
        "license",
    ] {
        if let Some(value) = package.get(key) {
            slim.insert(key.to_owned(), value.clone());
        }
    }
    slim.insert("dependencies".to_owned(), Value::Object(dependencies));
    Value::Object(slim)
}

/// The module rebon loads: `main`, else the `.` export, else `index.js`.
pub fn entry_of(package: &Value) -> String {
    if let Some(main) = package.get("main").and_then(Value::as_str) {
        return main.trim_start_matches("./").to_owned();
    }
    let dot = package.get("exports").and_then(|exports| exports.get("."));
    let from_exports = match dot {
        Some(Value::String(path)) => Some(path.clone()),
        Some(Value::Object(map)) => map
            .get("default")
            .or_else(|| map.get("import"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    from_exports
        .map(|path| path.trim_start_matches("./").to_owned())
        .unwrap_or_else(|| "index.js".to_owned())
}

/// What a package's own code says it reaches: the variables it reads through
/// its environment accessor and the hosts it names.
pub fn scan_requests(sources: &[String]) -> ContainerRequest {
    let env_re =
        regex::Regex::new(r#"environmentOf\([^)]*\)\s*\.\s*get\(\s*["'`]([A-Z][A-Z0-9_]*)["'`]"#)
            .expect("a literal pattern");
    let host_re = regex::Regex::new(r#"["'`]https://([a-z0-9][a-z0-9.-]*\.[a-z]{2,})(?:[/:"'`])"#)
        .expect("a literal pattern");
    let mut env = BTreeSet::new();
    let mut network = BTreeSet::new();
    for source in sources {
        for capture in env_re.captures_iter(source) {
            env.insert(capture[1].to_owned());
        }
        for capture in host_re.captures_iter(source) {
            let host = capture[1].to_ascii_lowercase();
            if !NOT_ENDPOINTS.contains(&host.as_str()) {
                network.insert(host);
            }
        }
    }
    ContainerRequest {
        network: network.into_iter().collect(),
        env: env.into_iter().collect(),
    }
}

/// The plugin id a package is known by: its name without the scope.
pub fn plugin_id_of(package: &Value) -> anyhow::Result<String> {
    let name = package
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("package.json names no package"))?;
    let id = name.rsplit('/').next().unwrap_or(name);
    if id.is_empty() {
        bail!("package name {name:?} has no plugin id");
    }
    Ok(id.to_owned())
}

/// The rebon manifest for a probed package.
pub fn manifest_for(
    package: &Value,
    probe: &Value,
    request: &ContainerRequest,
    config: Option<&Value>,
) -> anyhow::Result<Value> {
    let id = plugin_id_of(package)?;
    let names = |key: &str| -> Vec<Value> {
        probe
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let tools: Vec<Value> = names("tools")
        .iter()
        .filter_map(|tool| tool.get("name").cloned())
        .collect();
    let commands: Vec<Value> = names("commands")
        .iter()
        .filter_map(|command| command.get("name").cloned())
        .collect();
    let injects: Vec<&str> = ["required", "optional"]
        .iter()
        .filter_map(|key| {
            probe
                .get("inject")
                .and_then(|i| i.get(key))
                .and_then(Value::as_array)
        })
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    // The web seat answers a search or fetch through rebon's own tools when
    // no provider is configured, so a plugin using it may invoke those two.
    let invokable: Vec<&str> = if injects.contains(&"web") {
        vec!["WebSearch", "WebFetch"]
    } else {
        Vec::new()
    };
    let mut kernel = json!({
        "entry": entry_of(package),
        "tools": tools,
        "commands": commands,
        "services": names("services"),
        "llmProviders": names("llmProviders"),
        "eventTopics": names("eventTopics"),
        "seats": SEATS,
        "invokableTools": invokable,
    });
    if probe
        .get("report")
        .and_then(|report| report.get("sections"))
        .and_then(Value::as_array)
        .is_some_and(|sections| !sections.is_empty())
    {
        kernel["publishedTopics"] = json!([]);
    }
    let upstream = package.get("name").cloned().unwrap_or(Value::Null);
    let mut metadata = json!({
        "upstream": upstream,
        "upstreamVersion": package.get("version").cloned().unwrap_or(Value::Null),
        "adaptedFrom": "npm",
    });
    if let Some(config) = config {
        metadata["kernelPluginConfig"] = json!({ id.clone(): config });
    }
    let mut manifest = json!({
        "name": id.clone(),
        "version": package.get("version").and_then(Value::as_str).unwrap_or("0.0.0"),
        "description": package.get("description").cloned().unwrap_or(Value::Null),
        "capabilities": { "kernelPlugins": { id: kernel } },
        "metadata": metadata,
    });
    if !request.is_empty() {
        manifest["container"] = serde_json::to_value(request)?;
    }
    Ok(manifest)
}

/// Whether the probe saw a web fetch provider: one that reaches whatever
/// URL it is asked for, which no list of hosts can describe.
pub fn fetches_any_url(probe: &Value) -> bool {
    probe
        .get("services")
        .and_then(Value::as_array)
        .is_some_and(|services| {
            services
                .iter()
                .filter_map(Value::as_str)
                .any(|service| service.starts_with("web:fetch"))
        })
}

/// What [`adapt`] made of a package.
#[derive(Clone, Debug)]
pub struct Adapted {
    pub plugin_id: String,
    pub request: ContainerRequest,
    pub tools: Vec<String>,
    /// The skill roots a skill package declares (relative to the package).
    pub skills: Vec<String>,
}

/// The folders holding the `SKILL.md` skills a package ships, as the roots
/// rebon's skill loader reads (the parent of each skill folder), relative to
/// the package and with forward slashes. Its dependencies' are not its own.
pub fn skill_roots(dir: &Path) -> Vec<String> {
    let mut roots = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "node_modules" && name != ".git" {
                    stack.push(path);
                }
            } else if name.to_string_lossy().eq_ignore_ascii_case("SKILL.md") {
                let root = path.parent().and_then(Path::parent);
                if let Some(relative) = root.and_then(|root| root.strip_prefix(dir).ok()) {
                    let shown = relative.to_string_lossy().replace('\\', "/");
                    roots.insert(if shown.is_empty() {
                        ".".to_owned()
                    } else {
                        shown
                    });
                }
            }
        }
    }
    roots.into_iter().collect()
}

/// The manifest of a skill package: its skill roots, nothing that runs.
pub fn skills_manifest(package: &Value, roots: &[String]) -> anyhow::Result<Value> {
    let id = plugin_id_of(package)?;
    Ok(json!({
        "name": id,
        "version": package.get("version").and_then(Value::as_str).unwrap_or("0.0.0"),
        "description": package.get("description").cloned().unwrap_or(Value::Null),
        "capabilities": { "skills": roots },
        "compatibility": {
            "format": "rebon-plugin", "formatVersion": FORMAT_VERSION,
            "adapterRevision": ADAPTER_REVISION,
            "sdk": [{"name": "rebon-plugin-api", "range": "^1"}]
        },
        "metadata": {
            "upstream": package.get("name").cloned().unwrap_or(Value::Null),
            "upstreamVersion": package.get("version").cloned().unwrap_or(Value::Null),
            "adaptedFrom": "npm",
        },
    }))
}

/// The JavaScript a package ships, for [`scan_requests`]: its own files, not
/// its dependencies'.
fn package_sources(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "node_modules" && name != ".git" {
                    stack.push(path);
                }
            } else if path
                .extension()
                .is_some_and(|ext| ext == "js" || ext == "mjs" || ext == "cjs")
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push(text);
                }
            }
        }
    }
    out
}

fn npm_beside(node: &Path) -> PathBuf {
    let name = if cfg!(windows) { "npm.cmd" } else { "npm" };
    node.parent()
        .map(|dir| dir.join(name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Makes the npm package in `dir` a rebon package, in place.
///
/// `config` is the composition config a package cannot start without, when
/// the marketplace entry supplies one; it is probed with and recorded for
/// the install to list it with.
pub fn adapt(dir: &Path, config: Option<&Value>) -> anyhow::Result<Adapted> {
    let scripts =
        rebon_plugin_host::plugin_boot::plane_scripts().map_err(|error| anyhow!(error))?;
    let node = rebon_plugin_host::plugin_boot::resolve_node().map_err(|error| anyhow!(error))?;
    adapt_with(dir, config, &node, &scripts.compose_root)
}

/// [`adapt`] with the Node and runtime named.
pub fn adapt_with(
    dir: &Path,
    config: Option<&Value>,
    node: &Path,
    compose_root: &Path,
) -> anyhow::Result<Adapted> {
    let package_path = dir.join("package.json");
    let package: Value = serde_json::from_slice(
        &std::fs::read(&package_path)
            .with_context(|| format!("reading {}", package_path.display()))?,
    )
    .with_context(|| format!("{} is not JSON", package_path.display()))?;
    let id = plugin_id_of(&package)?;
    let declared = package
        .get("rebon")
        .map(|value| CompatibilityDeclaration::read(&id, Some(value), PluginFormat::DshNpm))
        .transpose()?;
    let skills = skill_roots(dir);
    if !skills.is_empty() {
        std::fs::write(
            dir.join("rebon-plugin.json"),
            serde_json::to_vec_pretty(&skills_manifest(&package, &skills)?)?,
        )?;
        return Ok(Adapted {
            plugin_id: id,
            request: ContainerRequest::default(),
            tools: Vec::new(),
            skills,
        });
    }
    let provided = provided_modules(compose_root)?;
    let mut compatibility = declared.unwrap_or_else(|| CompatibilityDeclaration {
        format: PluginFormat::DshNpm.name().to_owned(),
        format_version: FORMAT_VERSION,
        adapter_revision: ADAPTER_REVISION,
        sdk: Vec::new(),
        dsh_snapshot: None,
    });
    compatibility
        .sdk
        .extend(npm_sdk_requirements(&id, &package, &provided)?);
    compatibility.validate_sdk(&id, &host_sdk_versions())?;
    let mut slim = slim_package(&package, &provided);
    slim["rebon"] = serde_json::to_value(&compatibility)?;
    std::fs::write(&package_path, serde_json::to_vec_pretty(&slim)?)?;

    let has_dependencies = slim
        .get("dependencies")
        .and_then(Value::as_object)
        .is_some_and(|deps| !deps.is_empty());
    if has_dependencies {
        let output = Command::new(npm_beside(node))
            .current_dir(dir)
            .args([
                "install",
                "--omit=dev",
                "--legacy-peer-deps",
                "--ignore-scripts",
                "--no-bin-links",
                "--no-audit",
                "--no-fund",
                "--no-package-lock",
                "--loglevel=error",
            ])
            .output()
            .context("running npm (is it installed beside Node?)")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let missing = stderr
                .lines()
                .find(|line| line.contains("404") && line.contains("@deepseek-ai"))
                .map(|line| format!(" ({})", line.trim()))
                .unwrap_or_default();
            bail!(
                "{id}: npm could not install what it needs{missing}: {}",
                stderr.lines().rev().take(4).collect::<Vec<_>>().join(" | ")
            );
        }
    }

    // By where they really are: Node opens the entry by its real path and
    // checks that against the grants (see `container::real_path`).
    let real = rebon_plugin_host::container::real_path;
    let (compose_root, package_root) = (real(compose_root), real(dir));
    let runtime_root = compose_root
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", compose_root.display()))?;
    let entry = entry_of(&package);
    let mut probe = Command::new(node);
    probe
        .arg("--permission")
        .arg(format!("--allow-fs-read={}", runtime_root.display()))
        .arg(format!("--allow-fs-read={}", package_root.display()))
        .arg(compose_root.join("src").join("probe.mjs"))
        .arg(&package_root)
        .arg(&entry);
    if let Some(config) = config {
        probe.arg(config.to_string());
    }
    let output = probe.output().context("running the probe")?;
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned();
    let answer: Value = serde_json::from_str(&line).map_err(|_| {
        anyhow!(
            "{id}: the probe said nothing readable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    if answer.get("ok").and_then(Value::as_bool) != Some(true) {
        let missing: Vec<&str> = answer
            .get("missing")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let error = answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("it did not mount");
        if missing.is_empty() {
            bail!("{id} does not run on rebon: {error}");
        }
        bail!(
            "{id} needs services rebon does not offer ({}): {error}",
            missing.join(", ")
        );
    }
    let mut request = scan_requests(&package_sources(dir));
    if fetches_any_url(&answer) {
        request.network = vec![rebon_harness::rebon_plugin_package::container::ANY_HOST.to_owned()];
    }
    let mut manifest = manifest_for(&package, &answer, &request, config)?;
    manifest["compatibility"] = serde_json::to_value(compatibility)?;
    std::fs::write(
        dir.join("rebon-plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let tools = answer
        .get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(Adapted {
        plugin_id: id,
        request,
        tools,
        skills: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provided() -> BTreeSet<String> {
        [
            "@deepseek-ai/cordis",
            "@deepseek-ai/dsh-web",
            "@deepseek-ai/schemastery",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
    }

    #[test]
    fn the_slim_package_keeps_what_runs_and_the_peers_rebon_lacks() {
        let package = json!({
            "name": "@deepseek-ai/dsh-web-search-perplexity",
            "version": "0.0.1-rc.1",
            "main": "lib/index.js",
            "dependencies": { "@deepseek-ai/schemastery": "^3" },
            "peerDependencies": { "@deepseek-ai/cordis": "^4", "@deepseek-ai/dsh-web": "^0", "@deepseek-ai/dsh-invariants": "^0.0.1" },
            "devDependencies": { "@deepseek-ai/dsh-agent": "^0" },
            "scripts": { "postinstall": "evil" }
        });
        let slim = slim_package(&package, &provided());
        assert_eq!(
            slim["dependencies"],
            json!({ "@deepseek-ai/schemastery": "^3", "@deepseek-ai/dsh-invariants": "^0.0.1" })
        );
        assert!(slim.get("devDependencies").is_none());
        assert!(slim.get("peerDependencies").is_none());
        assert!(slim.get("scripts").is_none(), "nothing runs at install");
        assert_eq!(slim["main"], "lib/index.js");
    }

    #[test]
    fn the_entry_is_main_then_the_dot_export() {
        assert_eq!(
            entry_of(&json!({ "main": "./lib/index.js" })),
            "lib/index.js"
        );
        assert_eq!(
            entry_of(
                &json!({ "exports": { ".": { "types": "x.d.ts", "default": "./lib/a.js" } } })
            ),
            "lib/a.js"
        );
        assert_eq!(entry_of(&json!({})), "index.js");
    }

    #[test]
    fn a_package_asks_for_the_variables_and_hosts_its_code_names() {
        let source = r#"
            apiKey: config.apiKey ?? environmentOf(ctx).get("PERPLEXITY_API_KEY")?.value ?? "",
            const BASE = "https://api.perplexity.ai/chat";
            // see "https://github.com/deepseek-ai/harness" for details
            const schema = "https://json-schema.org/draft/2020-12/schema";
        "#;
        let request = scan_requests(&[source.to_owned()]);
        assert_eq!(request.env, vec!["PERPLEXITY_API_KEY".to_owned()]);
        assert_eq!(request.network, vec!["api.perplexity.ai".to_owned()]);
    }

    #[test]
    fn the_manifest_declares_what_the_probe_saw() {
        let package = json!({ "name": "@deepseek-ai/dsh-tool-web", "version": "0.0.1-rc.1", "main": "lib/index.js" });
        let probe = json!({
            "ok": true,
            "inject": { "required": ["tools", "web", "systemPrompt"], "optional": [] },
            "tools": [{ "name": "web_search", "description": "" }, { "name": "web_fetch", "description": "" }],
            "services": [], "llmProviders": [], "eventTopics": []
        });
        let manifest = manifest_for(&package, &probe, &ContainerRequest::default(), None).unwrap();
        let kernel = &manifest["capabilities"]["kernelPlugins"]["dsh-tool-web"];
        assert_eq!(manifest["name"], "dsh-tool-web");
        assert_eq!(kernel["entry"], "lib/index.js");
        assert_eq!(kernel["tools"], json!(["web_search", "web_fetch"]));
        assert_eq!(kernel["invokableTools"], json!(["WebSearch", "WebFetch"]));
        assert!(manifest.get("container").is_none(), "asks for nothing");
        // The manifest is one rebon reads.
        let parsed: rebon_harness::rebon_plugin_package::PluginManifest =
            serde_json::from_value(manifest).unwrap();
        assert!(parsed
            .capabilities
            .kernel_plugins
            .contains_key("dsh-tool-web"));
    }

    #[test]
    fn a_config_the_package_needs_rides_its_manifest() {
        let package = json!({ "name": "@deepseek-ai/dsh-tool-todo", "version": "1.0.0" });
        let probe = json!({ "ok": true, "tools": [{ "name": "todo_write" }] });
        let config = json!({ "allowParallelInProgress": false });
        let manifest = manifest_for(
            &package,
            &probe,
            &ContainerRequest::default(),
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            manifest["metadata"]["kernelPluginConfig"]["dsh-tool-todo"],
            config
        );
    }

    #[test]
    fn a_fetch_provider_is_one_that_reaches_any_url() {
        assert!(fetches_any_url(&json!({ "services": ["web:fetch:http"] })));
        assert!(!fetches_any_url(&json!({ "services": ["web:search:exa"] })));
        assert!(!fetches_any_url(&json!({})));
    }

    #[test]
    fn a_skill_package_declares_the_folders_its_skills_live_in() {
        let dir = tempfile::tempdir().unwrap();
        for skill in ["office-docx", "office-xlsx"] {
            let folder = dir.path().join("assets").join(skill);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("node_modules/dep/skills/inner")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/dep/skills/inner/SKILL.md"),
            "x",
        )
        .unwrap();
        assert_eq!(skill_roots(dir.path()), vec!["assets".to_owned()]);
        let manifest = skills_manifest(
            &json!({ "name": "@deepseek-ai/dsh-skill-office", "version": "0.0.1" }),
            &skill_roots(dir.path()),
        )
        .unwrap();
        let parsed: rebon_harness::rebon_plugin_package::PluginManifest =
            serde_json::from_value(manifest).unwrap();
        assert_eq!(parsed.name, "dsh-skill-office");
        assert_eq!(parsed.capabilities.skills.len(), 1);
        assert!(
            parsed.capabilities.kernel_plugins.is_empty(),
            "nothing of it runs"
        );
    }

    #[test]
    fn incompatible_npm_packages_are_refused_before_writing_or_probing() {
        use rebon_harness::rebon_plugin_package::compatibility::CompatibilityError;
        let runtime =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/compose-runtime");
        for case in [
            "unknown-sdk",
            "incompatible-sdk",
            "format",
            "adapter",
            "missing-sdk",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut package =
                json!({"name":"@deepseek-ai/dsh-demo", "peerDependencies":{"cordis":"^4"}});
            match case {
                "unknown-sdk" => {
                    package["peerDependencies"]["@deepseek-ai/dsh-session"] = json!("*")
                }
                "incompatible-sdk" => package["peerDependencies"]["cordis"] = json!("^4.0.0"),
                "format" | "adapter" => {
                    package["rebon"] =
                        json!({"format":"dsh-npm","formatVersion":1,"adapterRevision":1,"sdk":[]});
                    package["rebon"][if case == "format" {
                        "formatVersion"
                    } else {
                        "adapterRevision"
                    }] = json!(2);
                }
                "missing-sdk" => {
                    package.as_object_mut().unwrap().remove("peerDependencies");
                }
                _ => unreachable!(),
            }
            let original = serde_json::to_vec(&package).unwrap();
            std::fs::write(dir.path().join("package.json"), &original).unwrap();
            let error = adapt_with(dir.path(), None, &dir.path().join("missing-node"), &runtime)
                .unwrap_err();
            let structured = error
                .downcast_ref::<CompatibilityError>()
                .expect("compatibility is checked before starting Node or npm");
            assert!(matches!(
                (case, structured),
                (
                    "unknown-sdk" | "incompatible-sdk",
                    CompatibilityError::UnsupportedSdk { .. }
                ) | ("format", CompatibilityError::UnsupportedFormat { .. })
                    | ("adapter", CompatibilityError::UnsupportedAdapter { .. })
                    | (
                        "missing-sdk",
                        CompatibilityError::MissingCompatibility { .. }
                    )
            ));
            assert_eq!(
                std::fs::read(dir.path().join("package.json")).unwrap(),
                original
            );
            assert!(!dir.path().join("rebon-plugin.json").exists());
            assert!(!dir.path().join("node_modules").exists());
        }
    }

    #[test]
    fn a_cordis_package_preserves_sdk_evidence_after_the_probe() {
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("dsh-compatibility-probe");
        let Some(node) = std::env::var_os("REBON_TEST_NODE").map(PathBuf::from) else {
            assert!(
                std::env::var_os("REBON_REQUIRE_TEST_NODE").is_none(),
                "REBON_TEST_NODE must be set"
            );
            eprintln!("skipping: set REBON_TEST_NODE");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let runtime =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtimes/node/compose-runtime");
        std::fs::write(dir.path().join("package.json"), json!({
            "name":"@deepseek-ai/dsh-demo", "version":"1.0.0", "type":"module", "main":"index.mjs",
            "peerDependencies":{"cordis":"^4"}
        }).to_string()).unwrap();
        std::fs::write(
            dir.path().join("index.mjs"),
            "export function apply(ctx) {}\n",
        )
        .unwrap();
        let adapted = adapt_with(dir.path(), None, &node, &runtime).unwrap();
        assert_eq!(adapted.plugin_id, "dsh-demo");
        let slim: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("package.json")).unwrap())
                .unwrap();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("rebon-plugin.json")).unwrap())
                .unwrap();
        assert!(slim.get("peerDependencies").is_none());
        assert_eq!(slim["rebon"], manifest["compatibility"]);
        assert_eq!(
            manifest["compatibility"]["sdk"],
            json!([{"name":"cordis","range":"^4"}])
        );
        let parsed =
            rebon_harness::rebon_plugin_package::PluginManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(
            rebon_harness::rebon_plugin_package::compatibility::resolve_package(
                dir.path(),
                &parsed,
                None
            )
            .unwrap(),
            rebon_harness::rebon_plugin_package::compatibility::LoadCompatibility::Current(
                PluginFormat::DshNpm
            )
        );
    }

    #[test]
    fn static_skills_use_the_material_format_without_starting_node() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills/demo")).unwrap();
        std::fs::write(dir.path().join("skills/demo/SKILL.md"), "demo").unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            json!({"name":"@deepseek-ai/dsh-skills", "version":"1"}).to_string(),
        )
        .unwrap();
        let absent = dir.path().join("missing");
        let adapted = adapt_with(dir.path(), None, &absent, &absent).unwrap();
        assert_eq!(adapted.skills, vec!["skills"]);
        let parsed =
            rebon_harness::rebon_plugin_package::PluginManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(
            rebon_harness::rebon_plugin_package::compatibility::resolve_package(
                dir.path(),
                &parsed,
                None
            )
            .unwrap(),
            rebon_harness::rebon_plugin_package::compatibility::LoadCompatibility::Current(
                PluginFormat::Rebon
            )
        );
    }

    #[test]
    fn only_npm_cordis_packages_are_adapted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"@deepseek-ai/dsh-x"}"#,
        )
        .unwrap();
        assert!(is_cordis_npm_package(dir.path()));
        std::fs::write(dir.path().join("rebon-plugin.json"), "{}").unwrap();
        assert!(
            !is_cordis_npm_package(dir.path()),
            "a rebon package is not adapted"
        );
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("package.json"), r#"{"name":"left-pad"}"#).unwrap();
        assert!(!is_cordis_npm_package(other.path()));
    }
}

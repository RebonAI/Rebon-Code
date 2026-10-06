//! Containers: a plugin in a Node host of its own, confined.
//!
//! The process plane is one Node host every trusted entry shares — rebon's
//! own vendored packages, a composition the person wrote by hand. A plugin
//! installed from a marketplace is someone else's code, and it runs in a
//! container instead: a separate host whose Node runs under the permission
//! model, reading only the runtime's scripts and its own package, writing only
//! its own data directory, starting no processes, threads or native addons,
//! and seeing no environment variable it was not granted by name. What it
//! needs from the machine it asks rebon for, through the tools and seats the
//! plane already gates — which is where the person's permission is asked.
//!
//! Network is the one thing Node's permission model does not cover. A
//! container that declares hosts gets a proxy that admits exactly those, and
//! where the OS sandbox is usable the host runs under it with every other
//! connection refused (`rebon_tool::ContainerSandboxService`, provided by
//! the sandbox plugin). Without the OS layer the
//! proxy is advisory: `fetch` honours it, a raw socket does not.
//!
//! A container is also what makes install and uninstall immediate: its
//! process starts when its first entry loads and is gone when its last one
//! leaves, taking whatever the plugin had in memory with it — no restart, and
//! no stale module in Node's import cache the next time it loads.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rebon_plugin_supervisor::HostLauncher;
use rebon_tool::ConfinedLauncher;
use serde::{Deserialize, Serialize};

use crate::plugin_plane::ComposeEntry;

/// Where an entry runs when it does not run in the shared host.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContainerSpec {
    /// The container's name. Entries naming the same one share its host — the
    /// members of one installed bundle, which reach each other through Cordis
    /// services and so need one realm.
    pub id: String,
    /// What the host may read beyond the runtime's own scripts: the package.
    #[serde(default)]
    pub read: Vec<String>,
    /// The one directory the host may write, and its working directory.
    pub data_dir: String,
    /// The hosts it may connect to. Empty: none.
    #[serde(default)]
    pub network: Vec<String>,
    /// Environment variables handed through from rebon's own, by name — an
    /// API key the plugin was granted at install.
    #[serde(default)]
    pub env: Vec<String>,
}

/// The Node half of a container's launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerLaunch {
    pub node_args: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub working_directory: PathBuf,
    /// Every directory the host reads, in the order granted.
    pub read: Vec<PathBuf>,
}

/// Variables every host needs whatever it was granted: Windows will not
/// resolve a name or seed its crypto without `SystemRoot`.
const BASELINE_ENV: &[&str] = &["SystemRoot", "SYSTEMROOT", "windir", "LANG", "LC_ALL", "TZ"];

/// Variables the runtime itself reads, handed through when rebon has them.
const RUNTIME_ENV: &[&str] = &["REBON_KERNEL_JS_DIR"];

/// The directory the three script trees sit in, side by side:
/// `<root>/plugin-host/src/cli.mjs` → `<root>`.
pub fn runtime_root(host_script: &Path) -> Option<PathBuf> {
    host_script
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// A path as Node's permission model compares it: absolute, and without the
/// `\\?\` prefix Windows canonicalisation adds, which Node does not strip.
pub fn permission_path(path: &Path) -> PathBuf {
    let shown = path.to_string_lossy();
    match shown.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Builds the Node half of a container's launch.
///
/// `parent_env` reads rebon's own environment; nothing from it reaches the
/// host unless it is in the baseline, the runtime's own list or the spec's
/// grant. `confinement` adds what the OS layer needs (proxy variables).
pub fn container_launch(
    spec: &ContainerSpec,
    host_script: &Path,
    extra_read: &[PathBuf],
    parent_env: &dyn Fn(&str) -> Option<OsString>,
    confinement_env: &[(OsString, OsString)],
) -> ContainerLaunch {
    let data = permission_path(Path::new(&spec.data_dir));
    let mut read: Vec<PathBuf> = Vec::new();
    let mut grant = |path: PathBuf| {
        let path = permission_path(&path);
        if !read.contains(&path) {
            read.push(path);
        }
    };
    if let Some(root) = runtime_root(host_script) {
        grant(root);
    }
    for path in extra_read {
        grant(path.clone());
    }
    for path in &spec.read {
        grant(PathBuf::from(path));
    }
    grant(data.clone());

    let mut node_args: Vec<OsString> = vec!["--permission".into()];
    for path in &read {
        node_args.push(flag("--allow-fs-read=", path));
    }
    node_args.push(flag("--allow-fs-write=", &data));

    let mut environment: Vec<(OsString, OsString)> = Vec::new();
    let mut set = |key: &str, value: OsString| {
        if let Some(slot) = environment.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            environment.push((key.into(), value));
        }
    };
    for key in BASELINE_ENV.iter().chain(RUNTIME_ENV) {
        if let Some(value) = parent_env(key) {
            set(key, value);
        }
    }
    for key in &spec.env {
        if let Some(value) = parent_env(key) {
            set(key, value);
        }
    }
    // What a plugin's own environment accessor may show (the
    // `dsh-environment` shim): exactly the names granted, nothing ambient.
    if !spec.env.is_empty() {
        set("REBON_GRANTED_ENV", spec.env.join(",").into());
    }
    // Home and temp point into the container: a plugin that writes "~" or a
    // temp file lands where it is allowed to, and reads nobody else's.
    let tmp = data.join("tmp");
    for key in ["HOME", "USERPROFILE"] {
        set(key, data.clone().into_os_string());
    }
    for key in ["TMPDIR", "TEMP", "TMP"] {
        set(key, tmp.clone().into_os_string());
    }
    for (key, value) in confinement_env {
        set(&key.to_string_lossy(), value.clone());
    }
    // Node's own `fetch` ignores the proxy variables unless told — and a
    // container granted nothing has a proxy too, one that refuses everything,
    // so the flag follows the proxy rather than the grant.
    let proxied = confinement_env.iter().any(|(key, _)| {
        key.eq_ignore_ascii_case("https_proxy") || key.eq_ignore_ascii_case("http_proxy")
    });
    if proxied {
        set("NODE_USE_ENV_PROXY", "1".into());
    }

    ContainerLaunch {
        node_args,
        environment,
        working_directory: data,
        read,
    }
}

/// Which entries run in containers, and with what.
///
/// Everything rebon did not write and the person did not point at by hand:
/// an installed package, a mod folder discovered under the config home or a
/// mod directory variable. Rebon's own vendored packages and a module path in
/// `kernelPlugins.modules` stay on the shared host, and so does any id the
/// person lists in `kernelPlugins.trusted`.
#[derive(Clone, Debug, Default)]
pub struct Containment {
    config_dir: PathBuf,
    grants: rebon_plugin_package::container::ContainerGrants,
    trusted: std::collections::BTreeSet<String>,
}

impl Containment {
    /// Reads `kernelPlugins.trusted` and `plugins/grants.json` under
    /// `config_dir`. A missing or unreadable file trusts nothing and grants
    /// nothing.
    pub fn load(config_dir: &Path) -> Self {
        let trusted = std::fs::read(config_dir.join("config.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|config| {
                config
                    .get("kernelPlugins")?
                    .get("trusted")?
                    .as_array()
                    .cloned()
            })
            .map(|list| {
                list.iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            config_dir: config_dir.to_path_buf(),
            grants: rebon_plugin_package::container::ContainerGrants::load(config_dir),
            trusted,
        }
    }

    /// An installed package's entry, in the package's container.
    pub fn package(&self, entry: ComposeEntry) -> ComposeEntry {
        // The package's own name: an installed package sits in a versioned
        // folder (`plugins/<name>/<version>`), so the folder is not it.
        let package = rebon_plugin_package::PluginManifest::load_from_dir(Path::new(&entry.root))
            .map(|manifest| manifest.name)
            .unwrap_or_else(|_| entry.id.clone());
        let container = rebon_plugin_package::container::package_container_id(&package);
        self.contain(entry, &container)
    }

    /// A discovered mod's entry, in a container of its own.
    pub fn mod_entry(&self, entry: ComposeEntry) -> ComposeEntry {
        let container = rebon_plugin_package::container::mod_container_id(&entry.id);
        self.contain(entry, &container)
    }

    fn contain(&self, mut entry: ComposeEntry, container: &str) -> ComposeEntry {
        if self.trusted.contains(&entry.id) {
            return entry;
        }
        let grant = self.grants.get(container);
        let data = rebon_plugin_package::container::container_data_dir(&self.config_dir, container);
        entry.container = Some(ContainerSpec {
            id: container.to_owned(),
            read: vec![entry.root.clone()],
            data_dir: permission_path(&data).to_string_lossy().into_owned(),
            network: grant.network,
            env: grant.env,
        });
        entry
    }
}

/// The supervisor's launcher for what the OS layer answered.
pub fn host_launcher(confined: &ConfinedLauncher) -> HostLauncher {
    HostLauncher {
        program: confined.program.clone(),
        args: confined.args.clone(),
        env_set: confined.env_set.clone(),
        env_unset: confined.env_unset.clone(),
    }
}

fn flag(prefix: &str, path: &Path) -> OsString {
    let mut out = OsString::from(prefix);
    out.push(path.as_os_str());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            id: "snake@rebon".into(),
            read: vec!["/cfg/mods/snake".into()],
            data_dir: "/cfg/plugins/data/snake@rebon".into(),
            network: Vec::new(),
            env: vec!["EXA_API_KEY".into()],
        }
    }

    fn env(key: &str) -> Option<OsString> {
        match key {
            "EXA_API_KEY" => Some("exa-secret".into()),
            "ANTHROPIC_API_KEY" => Some("not-yours".into()),
            "SystemRoot" => Some(r"C:\Windows".into()),
            "PATH" => Some("/usr/bin".into()),
            _ => None,
        }
    }

    fn launch(spec: &ContainerSpec, confinement: &[(OsString, OsString)]) -> ContainerLaunch {
        container_launch(
            spec,
            Path::new("/rt/plugin-host/src/cli.mjs"),
            &[PathBuf::from("/rt/compose-runtime/payload")],
            &env,
            confinement,
        )
    }

    fn get<'a>(launch: &'a ContainerLaunch, key: &str) -> Option<&'a OsString> {
        launch
            .environment
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    #[test]
    fn the_host_reads_the_runtime_the_package_and_its_data_and_writes_only_its_data() {
        let launch = launch(&spec(), &[]);
        let args: Vec<String> = launch
            .node_args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--permission");
        let reads: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("--allow-fs-read="))
            .collect();
        assert_eq!(reads.len(), 4, "runtime, payload, package, data: {args:?}");
        assert!(args.contains(&format!("--allow-fs-read={}", Path::new("/rt").display())));
        let writes: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("--allow-fs-write="))
            .collect();
        assert_eq!(
            writes,
            vec![&format!(
                "--allow-fs-write={}",
                Path::new("/cfg/plugins/data/snake@rebon").display()
            )]
        );
        // No escape hatch was granted.
        assert!(!args.iter().any(|a| a.starts_with("--allow-child-process")
            || a.starts_with("--allow-worker")
            || a.starts_with("--allow-addons")));
        assert_eq!(
            launch.working_directory,
            PathBuf::from("/cfg/plugins/data/snake@rebon")
        );
    }

    #[test]
    fn only_granted_and_baseline_variables_cross_into_the_container() {
        let launch = launch(&spec(), &[]);
        assert_eq!(
            get(&launch, "EXA_API_KEY"),
            Some(&OsString::from("exa-secret"))
        );
        assert_eq!(
            get(&launch, "REBON_GRANTED_ENV"),
            Some(&OsString::from("EXA_API_KEY"))
        );
        assert_eq!(
            get(&launch, "SystemRoot"),
            Some(&OsString::from(r"C:\Windows"))
        );
        assert_eq!(
            get(&launch, "ANTHROPIC_API_KEY"),
            None,
            "a key it was not granted"
        );
        assert_eq!(get(&launch, "PATH"), None, "it starts no processes");
        assert_eq!(
            get(&launch, "HOME"),
            Some(&OsString::from("/cfg/plugins/data/snake@rebon"))
        );
        assert!(get(&launch, "TEMP").is_some_and(|v| v.to_string_lossy().ends_with("tmp")));
        assert_eq!(
            get(&launch, "NODE_USE_ENV_PROXY"),
            None,
            "no network, no proxy"
        );
    }

    #[test]
    fn a_container_with_hosts_reaches_them_through_the_proxy_it_was_given() {
        let mut spec = spec();
        spec.network = vec!["api.exa.ai".into()];
        let proxy = [(
            OsString::from("HTTPS_PROXY"),
            OsString::from("http://127.0.0.1:7"),
        )];
        let launch = launch(&spec, &proxy);
        assert_eq!(
            get(&launch, "HTTPS_PROXY"),
            Some(&OsString::from("http://127.0.0.1:7"))
        );
        assert_eq!(
            get(&launch, "NODE_USE_ENV_PROXY"),
            Some(&OsString::from("1"))
        );
    }

    #[test]
    fn a_container_granted_no_network_still_uses_its_proxy_which_refuses_everything() {
        // Without the flag Node's fetch would ignore the deny-all proxy and
        // connect directly.
        let proxy = [(
            OsString::from("HTTPS_PROXY"),
            OsString::from("http://127.0.0.1:7"),
        )];
        let launch = launch(&spec(), &proxy);
        assert_eq!(
            get(&launch, "NODE_USE_ENV_PROXY"),
            Some(&OsString::from("1"))
        );
    }

    #[test]
    fn a_read_root_granted_twice_is_granted_once() {
        let mut spec = spec();
        spec.read.push("/rt".into());
        let launch = launch(&spec, &[]);
        assert_eq!(
            launch
                .read
                .iter()
                .filter(|p| *p == Path::new("/rt"))
                .count(),
            1
        );
    }

    #[test]
    fn windows_verbatim_prefixes_are_dropped_and_unc_paths_kept() {
        assert_eq!(
            permission_path(Path::new(r"\\?\C:\Users\a\mods")),
            PathBuf::from(r"C:\Users\a\mods")
        );
        assert_eq!(
            permission_path(Path::new(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\?\UNC\server\share")
        );
    }

    fn installed(id: &str, root: &str) -> ComposeEntry {
        ComposeEntry {
            id: id.into(),
            root: root.into(),
            entry: "index.js".into(),
            ..ComposeEntry::default()
        }
    }

    #[test]
    fn an_installed_package_runs_in_its_container_with_what_was_granted() {
        let dir = tempfile::tempdir().unwrap();
        let mut grants = rebon_plugin_package::container::ContainerGrants::default();
        grants.set(
            "pkg-dsh-web-search-exa",
            rebon_plugin_package::container::ContainerGrant {
                network: vec!["api.exa.ai".into()],
                env: Vec::new(),
            },
        );
        grants.save(dir.path()).unwrap();
        let containment = Containment::load(dir.path());
        // Installed in a versioned folder: the container is named after the
        // package, not the folder.
        let root = dir.path().join("plugins/dsh-web-search-exa/0.1.0");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("rebon-plugin.json"),
            serde_json::json!({ "name": "dsh-web-search-exa", "version": "0.1.0" }).to_string(),
        )
        .unwrap();
        let root = root.to_string_lossy().into_owned();
        let entry = containment.package(installed("dsh-web-search-exa", &root));
        let spec = entry.container.expect("contained");
        assert_eq!(spec.id, "pkg-dsh-web-search-exa");
        assert_eq!(spec.read, vec![root]);
        assert_eq!(spec.network, vec!["api.exa.ai".to_string()]);
        assert!(spec
            .data_dir
            .replace('\\', "/")
            .ends_with("plugins/data/pkg-dsh-web-search-exa"));
    }

    #[test]
    fn a_mod_gets_a_container_of_its_own_and_nothing_granted_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let entry = Containment::load(dir.path()).mod_entry(installed("snake", "/cfg/mods/snake"));
        let spec = entry.container.expect("contained");
        assert_eq!(spec.id, "mod-snake");
        assert!(spec.network.is_empty() && spec.env.is_empty());
    }

    #[test]
    fn a_trusted_id_stays_on_the_shared_host() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            serde_json::json!({ "kernelPlugins": { "trusted": ["snake"] } }).to_string(),
        )
        .unwrap();
        let containment = Containment::load(dir.path());
        assert!(containment
            .mod_entry(installed("snake", "/m/snake"))
            .container
            .is_none());
        assert!(containment
            .mod_entry(installed("other", "/m/other"))
            .container
            .is_some());
    }

    #[test]
    fn the_runtime_root_is_three_levels_above_the_host_script() {
        assert_eq!(
            runtime_root(Path::new("/rt/plugin-host/src/cli.mjs")),
            Some(PathBuf::from("/rt"))
        );
    }

    #[test]
    fn a_spec_round_trips_and_refuses_unknown_fields() {
        let value = serde_json::to_value(spec()).unwrap();
        assert_eq!(value["dataDir"], "/cfg/plugins/data/snake@rebon");
        let back: ContainerSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back, spec());
        let unknown = serde_json::json!({"id": "x", "dataDir": "/d", "shell": true});
        assert!(serde_json::from_value::<ContainerSpec>(unknown).is_err());
    }
}

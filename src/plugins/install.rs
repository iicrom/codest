//! External plugin installation: `codest plugin install|uninstall|list|build`.
//!
//! Linking a plugin is compile-time work — *something* has to mention
//! `&SOME_PLUGIN` in the source that becomes the binary. To keep that
//! something out of the checked-out repository, the installer owns a small
//! generated crate under `$CODEST_HOME`:
//!
//! ```text
//! ~/.codest/plugins.toml          the plugin list (machine-managed)
//! ~/.codest/plugins/crate/        a standalone Cargo package
//!     Cargo.toml                  codest + one line per plugin
//!     src/main.rs                 static LOCAL_PLUGINS: &[&dyn Plugin] = &[…]
//!     src/bin/cst.rs              the alias, same list
//! ```
//!
//! `cargo build` there produces `codest`/`cst` binaries with those plugins
//! linked in. Nothing in the codest checkout is touched, so `git status` stays
//! clean and `git update-index --skip-worktree` is no longer needed.
//!
//! The one hard constraint is *contract identity*: the host and every plugin
//! must resolve `cst-plugin-api` to the same crate, or `&QoderPlugin` is not a
//! `&dyn Plugin`. A plugin that names the contract by `path` points at the
//! same directory the host uses, so unification is automatic; for any other
//! plugin the generated manifest carries `[patch.crates-io]`, which pins the
//! published name to that same directory. See [`InstallState::needs_patch`].

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde::Serialize;

use crate::core::config::ConfigError;
use crate::core::config::write_checked;
use crate::core::rollout::codest_home;

/// State file under `$CODEST_HOME`.
const STATE_FILE: &str = "plugins.toml";
/// Generated crate, relative to `$CODEST_HOME`.
const CRATE_SUBDIR: &str = "plugins/crate";
/// Cargo package name of the generated crate.
const WRAPPER_PACKAGE: &str = "codest-local";
/// The contract crate the host and plugins must share.
const CONTRACT_CRATE: &str = "cst-plugin-api";
const CONTRACT_SUBDIR: &str = "crates/cst-plugin-api";
/// Exported `static` a plugin is assumed to contribute when it declares none.
const DEFAULT_SYMBOL: &str = "PLUGIN";

/// Where a plugin comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Path,
    Git,
    Registry,
}

/// One plugin in the generated wrapper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPlugin {
    /// Cargo package name, also the dependency key in the manifest.
    pub name: String,
    /// Identifier the generated code refers to (`-` → `_`).
    pub crate_name: String,
    /// Exported `static` values to link, in declaration order.
    #[serde(default = "default_symbols")]
    pub symbols: Vec<String>,
    /// A checkout to depend on by path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A repository to depend on by git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Version requirement for a registry install; `None` means `*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The plugin's own `cst-plugin-api` dependency when it is `path`-based,
    /// absolutized. `None` means the generated manifest must emit
    /// `[patch.crates-io]` so the contract still unifies — see
    /// [`InstallState::needs_patch`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_pinned_path: Option<String>,
}

fn default_symbols() -> Vec<String> {
    vec![DEFAULT_SYMBOL.to_string()]
}

impl InstalledPlugin {
    /// Which of the three sources this entry uses.
    ///
    /// [`InstalledPlugin::validate`] rejects an entry with zero or several.
    pub fn kind(&self) -> SourceKind {
        if self.git.is_some() {
            SourceKind::Git
        } else if self.path.is_some() {
            SourceKind::Path
        } else {
            SourceKind::Registry
        }
    }

    /// The `[dependencies]` line this plugin contributes.
    pub fn dependency_line(&self) -> String {
        match self.kind() {
            SourceKind::Path => format!(
                "{} = {{ path = {} }}",
                self.name,
                toml_string(self.path.as_deref().unwrap_or_default())
            ),
            SourceKind::Git => {
                let mut line = format!(
                    "{} = {{ git = {}",
                    self.name,
                    toml_string(self.git.as_deref().unwrap_or_default())
                );
                // Cargo takes at most one of these; `validate` enforces it.
                for (key, value) in [
                    ("rev", &self.rev),
                    ("branch", &self.branch),
                    ("tag", &self.tag),
                ] {
                    if let Some(value) = value {
                        line.push_str(&format!(", {key} = {}", toml_string(value)));
                    }
                }
                line.push_str(" }");
                line
            }
            SourceKind::Registry => format!(
                "{} = {}",
                self.name,
                toml_string(self.version.as_deref().unwrap_or("*"))
            ),
        }
    }

    /// Reject states the manifest renderer cannot express.
    pub fn validate(&self) -> Result<(), InstallError> {
        let kinds = [self.path.is_some(), self.git.is_some()];
        if kinds.iter().filter(|used| **used).count() > 1 {
            return Err(InstallError::Invalid {
                message: format!(
                    "plugin `{}` declares more than one source (path, git, registry)",
                    self.name
                ),
            });
        }
        let refs = [&self.rev, &self.branch, &self.tag];
        if refs.iter().filter(|value| value.is_some()).count() > 1 {
            return Err(InstallError::Invalid {
                message: format!(
                    "plugin `{}` declares more than one of rev/branch/tag",
                    self.name
                ),
            });
        }
        if (self.rev.is_some() || self.branch.is_some() || self.tag.is_some())
            && self.git.is_none()
        {
            return Err(InstallError::Invalid {
                message: format!(
                    "plugin `{}` declares a git revision without a git source",
                    self.name
                ),
            });
        }
        if self.version.is_some() && self.git.is_some() {
            return Err(InstallError::Invalid {
                message: format!(
                    "plugin `{}` is a git dependency and cannot also pin a version",
                    self.name
                ),
            });
        }
        validate_crate_name(&self.crate_name)?;
        if self.symbols.is_empty() {
            return Err(InstallError::Invalid {
                message: format!("plugin `{}` declares no exported symbols", self.name),
            });
        }
        Ok(())
    }
}

/// The whole `plugins.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    /// The codest checkout the wrapper depends on. Recorded so `build` and
    /// `uninstall` keep working after the binary was `cargo install`ed
    /// somewhere the compile-time source path no longer applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codest_src: Option<String>,
    #[serde(default)]
    pub plugins: Vec<InstalledPlugin>,
}

impl InstallState {
    /// Whether the generated manifest needs `[patch.crates-io]`.
    ///
    /// True as soon as one plugin does not name the contract by `path`: a
    /// registry or git dependency on `cst-plugin-api` would otherwise resolve
    /// to a *second* copy of the contract crate, and the trait objects would
    /// not match. A `path`-pinned plugin already shares the host's copy.
    pub fn needs_patch(&self) -> bool {
        self.plugins
            .iter()
            .any(|plugin| plugin.contract_pinned_path.is_none())
    }

    fn insert(&mut self, plugin: InstalledPlugin) -> bool {
        match self
            .plugins
            .iter_mut()
            .find(|existing| existing.name == plugin.name)
        {
            Some(existing) => {
                *existing = plugin;
                true
            }
            None => {
                self.plugins.push(plugin);
                self.plugins.sort_by(|a, b| a.name.cmp(&b.name));
                false
            }
        }
    }

    fn remove(&mut self, name: &str) -> bool {
        let before = self.plugins.len();
        self.plugins.retain(|plugin| plugin.name != name);
        self.plugins.len() != before
    }

    fn validate(&self) -> Result<(), InstallError> {
        for plugin in &self.plugins {
            plugin.validate()?;
        }
        let mut names: Vec<&str> = Vec::with_capacity(self.plugins.len());
        for plugin in &self.plugins {
            if names.contains(&plugin.name.as_str()) {
                return Err(InstallError::Invalid {
                    message: format!("plugin `{}` is listed twice", plugin.name),
                });
            }
            names.push(&plugin.name);
        }
        let mut crates: Vec<&str> = Vec::with_capacity(self.plugins.len());
        for plugin in &self.plugins {
            if crates.contains(&plugin.crate_name.as_str()) {
                return Err(InstallError::Invalid {
                    message: format!(
                        "plugins `{}` and another entry both use the crate name `{}`",
                        plugin.name, plugin.crate_name
                    ),
                });
            }
            crates.push(&plugin.crate_name);
        }
        Ok(())
    }
}

/// What `install` was asked for.
#[derive(Clone, Debug, Default)]
pub struct InstallRequest {
    /// A checkout path, a git URL, or a crates.io package name.
    pub target: Option<String>,
    /// Treat the target as the package name of a repository at this URL.
    pub git: Option<String>,
    /// Package name override (required for `--git` without a target).
    pub package: Option<String>,
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    /// Version requirement for a registry install.
    pub version: Option<String>,
    /// Exported statics to link; empty means the manifest's metadata, else
    /// `["PLUGIN"]`.
    pub symbols: Vec<String>,
    /// Crate identifier override.
    pub crate_name: Option<String>,
    /// The codest checkout to depend on.
    pub codest_src: Option<PathBuf>,
}

/// What `install` did.
#[derive(Clone, Debug)]
pub struct InstallOutcome {
    pub plugin: InstalledPlugin,
    /// `true` when an entry with the same package name was replaced.
    pub replaced: bool,
    pub state_path: PathBuf,
    pub crate_dir: PathBuf,
    /// The built binary, when the caller asked for a build.
    pub binary: Option<PathBuf>,
}

/// What `uninstall` did.
#[derive(Clone, Debug)]
pub struct UninstallOutcome {
    pub name: String,
    pub remaining: usize,
    pub crate_dir: PathBuf,
    pub binary: Option<PathBuf>,
}

/// What `build` did.
#[derive(Clone, Debug)]
pub struct BuildOutcome {
    pub crate_dir: PathBuf,
    pub profile: &'static str,
    pub binaries: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot serialize {path}: {message}")]
    Serialize { path: PathBuf, message: String },
    #[error("invalid plugin list: {message}")]
    Invalid { message: String },
    #[error("`{name}` is not a valid crate name (use lowercase ASCII, digits, `_`)")]
    InvalidCrateName { name: String },
    #[error("{path} is not a plugin checkout: {reason}")]
    NotAPlugin { path: PathBuf, reason: String },
    #[error(
        "no codest checkout at {path}: {reason} (pass --codest-src, or set CODEST_SRC, \
         to the directory holding codest's Cargo.toml)"
    )]
    NotACodestSource { path: PathBuf, reason: String },
    #[error(
        "no plugin named `{name}` is installed (installed: {known})"
    )]
    NotInstalled { name: String, known: String },
    #[error(
        "plugin `{name}` pins `{contract}` at {path}, which is not the host's copy at {expected}; \
         the two copies would not share the `Plugin` trait"
    )]
    ContractMismatch {
        name: String,
        contract: String,
        path: PathBuf,
        expected: PathBuf,
    },
    #[error("{path} is locked by another `codest plugin` run; remove it if none is running")]
    Locked { path: PathBuf },
    #[error("cargo is not on PATH: {source}")]
    CargoMissing { source: std::io::Error },
    #[error(
        "`cargo {operation}` failed for {path} (exit status {status}); \
         the plugin list was not changed"
    )]
    Build {
        operation: &'static str,
        path: PathBuf,
        status: String,
    },
}

/// `$CODEST_HOME/plugins.toml`.
pub fn state_path(home: &Path) -> PathBuf {
    home.join(STATE_FILE)
}

/// `$CODEST_HOME/plugins/crate`.
pub fn crate_dir(home: &Path) -> PathBuf {
    home.join(CRATE_SUBDIR)
}

/// Load the plugin list; a missing file is an empty list.
pub fn load_state(home: &Path) -> Result<InstallState, InstallError> {
    let path = state_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstallState::default());
        }
        Err(source) => return Err(InstallError::Read { path, source }),
    };
    let state: InstallState = toml::from_str(&text).map_err(|source| InstallError::Parse {
        path: path.clone(),
        source,
    })?;
    state.validate()?;
    Ok(state)
}

/// Write the plugin list atomically.
fn save_state(home: &Path, state: &InstallState) -> Result<(), InstallError> {
    let path = state_path(home);
    let text = toml::to_string_pretty(state).map_err(|source| InstallError::Serialize {
        path: path.clone(),
        message: source.to_string(),
    })?;
    debug_assert_eq!(
        toml::from_str::<InstallState>(&text).ok(),
        Some(state.clone()),
        "plugins.toml must round-trip"
    );
    write_checked(&path, &text)?;
    Ok(())
}

/// Install (or reinstall) one plugin and rebuild the wrapper.
pub fn install(request: &InstallRequest, build: bool) -> Result<InstallOutcome, InstallError> {
    install_in(&codest_home(), request, build)
}

/// [`install`] against an explicit home directory.
pub fn install_in(
    home: &Path,
    request: &InstallRequest,
    build: bool,
) -> Result<InstallOutcome, InstallError> {
    let _lock = StateLock::acquire(home)?;
    let previous = load_state(home)?;
    let codest_src = resolve_codest_src(request.codest_src.as_deref(), previous.codest_src.as_deref())?;
    let plugin = resolve_plugin(request, &codest_src)?;

    let mut state = previous.clone();
    state.codest_src = Some(path_string(&codest_src));
    let replaced = state.insert(plugin.clone());
    state.validate()?;

    let crate_dir = crate_dir(home);
    let state_path = state_path(home);
    save_state(home, &state)?;
    if let Err(error) = generate(&crate_dir, &state, &state_path) {
        // The plugin list is the source of truth; a crate that cannot be
        // written must not leave a state file pointing at nothing.
        let _ = save_state(home, &previous);
        return Err(error);
    }

    let binary = if build {
        match build_wrapper(&crate_dir, false) {
            Ok(binary) => Some(binary),
            Err(error) => {
                let _ = save_state(home, &previous);
                let _ = generate(&crate_dir, &previous, &state_path);
                return Err(error);
            }
        }
    } else {
        None
    };

    Ok(InstallOutcome {
        plugin,
        replaced,
        state_path,
        crate_dir,
        binary,
    })
}

/// Remove a plugin and rebuild the wrapper.
pub fn uninstall(name: &str, build: bool) -> Result<UninstallOutcome, InstallError> {
    uninstall_in(&codest_home(), name, build)
}

/// [`uninstall`] against an explicit home directory.
pub fn uninstall_in(
    home: &Path,
    name: &str,
    build: bool,
) -> Result<UninstallOutcome, InstallError> {
    let _lock = StateLock::acquire(home)?;
    let previous = load_state(home)?;
    let mut state = previous.clone();
    if !state.remove(name) {
        let known = if previous.plugins.is_empty() {
            "none".to_string()
        } else {
            previous
                .plugins
                .iter()
                .map(|plugin| plugin.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(InstallError::NotInstalled {
            name: name.to_string(),
            known,
        });
    }

    // The generated manifest needs a codest checkout even when nothing is
    // installed, so the source is resolved before the rewrite rather than
    // only for the build step.
    let src = resolve_codest_src(None, state.codest_src.as_deref())?;
    state.codest_src = Some(path_string(&src));

    let crate_dir = crate_dir(home);
    let state_path = state_path(home);
    save_state(home, &state)?;
    if let Err(error) = generate(&crate_dir, &state, &state_path) {
        let _ = save_state(home, &previous);
        return Err(error);
    }

    let binary = if build {
        match build_wrapper(&crate_dir, false) {
            Ok(binary) => Some(binary),
            Err(error) => {
                let _ = save_state(home, &previous);
                let _ = generate(&crate_dir, &previous, &state_path);
                return Err(error);
            }
        }
    } else {
        None
    };

    Ok(UninstallOutcome {
        name: name.to_string(),
        remaining: state.plugins.len(),
        crate_dir,
        binary,
    })
}

/// Regenerate the wrapper from the saved list and build it.
pub fn build(
    codest_src: Option<&Path>,
    release: bool,
    install: bool,
) -> Result<BuildOutcome, InstallError> {
    build_in(&codest_home(), codest_src, release, install)
}

/// [`build`] against an explicit home directory.
pub fn build_in(
    home: &Path,
    codest_src: Option<&Path>,
    release: bool,
    install_into_cargo_bin: bool,
) -> Result<BuildOutcome, InstallError> {
    let _lock = StateLock::acquire(home)?;
    let mut state = load_state(home)?;
    let src = resolve_codest_src(codest_src, state.codest_src.as_deref())?;
    state.codest_src = Some(path_string(&src));

    let crate_dir = crate_dir(home);
    let state_path = state_path(home);
    save_state(home, &state)?;
    generate(&crate_dir, &state, &state_path)?;

    if install_into_cargo_bin {
        run_cargo(
            &["install", "--path", &path_string(&crate_dir), "--force"],
            "install",
            &crate_dir,
        )?;
        return Ok(BuildOutcome {
            crate_dir,
            profile: if release { "release" } else { "debug" },
            binaries: Vec::new(),
        });
    }

    let binary = build_wrapper(&crate_dir, release)?;
    let profile = if release { "release" } else { "debug" };
    let alias = binary.with_file_name(executable_name("cst"));
    Ok(BuildOutcome {
        crate_dir,
        profile,
        binaries: vec![binary, alias],
    })
}

/// Read the saved plugin list.
pub fn list() -> Result<InstallState, InstallError> {
    load_state(&codest_home())
}

/// Resolve which codest checkout the wrapper should depend on.
///
/// Precedence: the explicit path, then `CODEST_SRC`, then the directory this
/// binary was compiled from (a source build), then the path recorded in
/// `plugins.toml` (an installed binary). Every candidate is verified, so a
/// stale one degrades to the next instead of producing a broken manifest.
pub fn resolve_codest_src(
    explicit: Option<&Path>,
    recorded: Option<&str>,
) -> Result<PathBuf, InstallError> {
    let mut rejected: Vec<(PathBuf, String)> = Vec::new();
    let candidates: Vec<PathBuf> = explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::var_os("CODEST_SRC").map(PathBuf::from))
        .chain(Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))))
        .chain(recorded.map(PathBuf::from))
        .collect();

    for candidate in candidates {
        match check_codest_src(&candidate) {
            Ok(()) => return Ok(canonical(&candidate)),
            Err(reason) => {
                if explicit.is_some() {
                    return Err(InstallError::NotACodestSource {
                        path: candidate,
                        reason,
                    });
                }
                rejected.push((candidate, reason));
            }
        }
    }

    Err(InstallError::NotACodestSource {
        path: rejected
            .first()
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| PathBuf::from(".")),
        reason: rejected
            .into_iter()
            .map(|(path, reason)| format!("{}: {reason}", path.display()))
            .collect::<Vec<_>>()
            .join("; "),
    })
}

fn check_codest_src(path: &Path) -> Result<(), String> {
    let manifest = path.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|_| format!("no Cargo.toml ({})", manifest.display()))?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
    let name = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str());
    if name != Some("codest") {
        return Err("Cargo.toml does not declare package `codest`".to_string());
    }
    if !path.join(CONTRACT_SUBDIR).join("Cargo.toml").is_file() {
        return Err(format!("missing {CONTRACT_SUBDIR}"));
    }
    Ok(())
}

/// Turn a request into a concrete plugin entry.
fn resolve_plugin(
    request: &InstallRequest,
    codest_src: &Path,
) -> Result<InstalledPlugin, InstallError> {
    let target = request.target.as_deref();

    if let Some(url) = request.git.as_deref() {
        let name = request
            .package
            .clone()
            .or_else(|| target.map(str::to_string))
            .ok_or_else(|| InstallError::Invalid {
                message: "`--git` needs a package name: `codest plugin install <name> --git <url>`"
                    .to_string(),
            })?;
        let mut plugin = InstalledPlugin {
            crate_name: crate_ident(&name),
            name,
            symbols: default_symbols(),
            path: None,
            git: Some(url.to_string()),
            rev: request.rev.clone(),
            branch: request.branch.clone(),
            tag: request.tag.clone(),
            version: None,
            contract_pinned_path: None,
        };
        apply_overrides(&mut plugin, request);
        plugin.validate()?;
        return Ok(plugin);
    }

    let target = target.ok_or_else(|| InstallError::Invalid {
        message: "nothing to install: pass a checkout path, a package name, or --git <url>"
            .to_string(),
    })?;

    if looks_like_git_url(target) {
        let name = request
            .package
            .clone()
            .or_else(|| package_from_url(target))
            .ok_or_else(|| InstallError::Invalid {
                message: format!("cannot infer a package name from `{target}`; pass --package"),
            })?;
        let mut plugin = InstalledPlugin {
            crate_name: crate_ident(&name),
            name,
            symbols: default_symbols(),
            path: None,
            git: Some(target.to_string()),
            rev: request.rev.clone(),
            branch: request.branch.clone(),
            tag: request.tag.clone(),
            version: None,
            contract_pinned_path: None,
        };
        apply_overrides(&mut plugin, request);
        plugin.validate()?;
        return Ok(plugin);
    }

    let candidate = PathBuf::from(target);
    if candidate.is_dir() || looks_like_path(target) {
        let dir = canonical(&candidate);
        let facts = read_plugin_facts(&dir)?;
        let contract = facts.contract_path.map(|path| canonical(&path));
        if let Some(contract) = &contract {
            let expected = canonical(&codest_src.join(CONTRACT_SUBDIR));
            if contract != &expected {
                return Err(InstallError::ContractMismatch {
                    name: facts.name,
                    contract: CONTRACT_CRATE.to_string(),
                    path: contract.clone(),
                    expected,
                });
            }
        }
        let mut plugin = InstalledPlugin {
            crate_name: crate_ident(&facts.name),
            name: facts.name,
            symbols: if facts.symbols.is_empty() {
                default_symbols()
            } else {
                facts.symbols
            },
            path: Some(path_string(&dir)),
            git: None,
            rev: None,
            branch: None,
            tag: None,
            version: None,
            contract_pinned_path: contract.map(|path| path_string(&path)),
        };
        apply_overrides(&mut plugin, request);
        plugin.validate()?;
        return Ok(plugin);
    }

    // Anything else is a published package name.
    let mut plugin = InstalledPlugin {
        crate_name: crate_ident(target),
        name: target.to_string(),
        symbols: default_symbols(),
        path: None,
        git: None,
        rev: None,
        branch: None,
        tag: None,
        version: Some(
            request
                .version
                .clone()
                .unwrap_or_else(|| "*".to_string()),
        ),
        contract_pinned_path: None,
    };
    apply_overrides(&mut plugin, request);
    plugin.validate()?;
    Ok(plugin)
}

/// Command-line overrides and conflicts that `resolve_plugin` cannot know
/// about (they only matter once a source kind is settled).
fn apply_overrides(plugin: &mut InstalledPlugin, request: &InstallRequest) {
    if let Some(crate_name) = &request.crate_name {
        plugin.crate_name = crate_name.clone();
    }
    if !request.symbols.is_empty() {
        plugin.symbols = request.symbols.clone();
    }
}

/// What a plugin checkout declares about itself.
struct PluginFacts {
    name: String,
    /// Exported statics from `[package.metadata.codest] plugins`.
    symbols: Vec<String>,
    /// The plugin's `cst-plugin-api` dependency when it is `path`-based.
    contract_path: Option<PathBuf>,
}

fn read_plugin_facts(dir: &Path) -> Result<PluginFacts, InstallError> {
    let manifest = dir.join("Cargo.toml");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(source) => {
            return Err(InstallError::NotAPlugin {
                path: dir.to_path_buf(),
                reason: format!("cannot read Cargo.toml: {source}"),
            });
        }
    };
    let value: toml::Value = toml::from_str(&text).map_err(|source| InstallError::Parse {
        path: manifest.clone(),
        source,
    })?;
    let name = value
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        .ok_or_else(|| InstallError::NotAPlugin {
            path: dir.to_path_buf(),
            reason: "Cargo.toml has no [package] name".to_string(),
        })?
        .to_string();

    let symbols = value
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("codest"))
        .and_then(|codest| codest.get("plugins"))
        .and_then(|plugins| plugins.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let contract_path = value
        .get("dependencies")
        .and_then(|deps| deps.get(CONTRACT_CRATE))
        .and_then(|dep| dep.get("path"))
        .and_then(|path| path.as_str())
        .map(|path| {
            // A relative path is relative to the plugin's own manifest.
            canonical(&dir.join(path))
        });

    Ok(PluginFacts {
        name,
        symbols,
        contract_path,
    })
}

/// Write the generated crate: manifest, both binaries, and its `.gitignore`.
fn generate(
    dir: &Path,
    state: &InstallState,
    state_path: &Path,
) -> Result<(), InstallError> {
    let bin_dir = dir.join("src/bin");
    std::fs::create_dir_all(&bin_dir).map_err(|source| InstallError::Write {
        path: bin_dir.clone(),
        source,
    })?;

    let files = [
        ("Cargo.toml", render_manifest(dir, state, state_path)),
        ("src/main.rs", render_binary(state, "codest")),
        ("src/bin/cst.rs", render_binary(state, "cst")),
        (".gitignore", "/target\n".to_string()),
    ];
    for (relative, text) in files {
        let path = dir.join(relative);
        std::fs::write(&path, text).map_err(|source| InstallError::Write { path, source })?;
    }
    Ok(())
}

/// The generated `Cargo.toml`.
fn render_manifest(dir: &Path, state: &InstallState, state_path: &Path) -> String {
    let mut out = String::new();
    out.push_str("# GENERATED by `codest plugin install` — do not edit.\n");
    out.push_str(&format!(
        "# Source of truth: {}\n",
        state_path.display()
    ));
    out.push_str(&format!(
        "# Rebuild with: cargo build --manifest-path {}\n\n",
        dir.join("Cargo.toml").display()
    ));
    out.push_str("[package]\n");
    out.push_str(&format!("name = \"{WRAPPER_PACKAGE}\"\n"));
    out.push_str("version = \"0.0.0\"\n");
    out.push_str("edition = \"2024\"\n");
    out.push_str("rust-version = \"1.88\"\n");
    out.push_str("publish = false\n\n");
    out.push_str(
        "# Its own workspace root: the generated crate must not be absorbed into\n\
         # a workspace that happens to be an ancestor of $CODEST_HOME.\n\
         [workspace]\n\n",
    );
    out.push_str("[[bin]]\nname = \"codest\"\npath = \"src/main.rs\"\n\n");
    out.push_str("[[bin]]\nname = \"cst\"\npath = \"src/bin/cst.rs\"\n\n");

    out.push_str("[dependencies]\n");
    out.push_str(&format!(
        "codest = {{ path = {} }}\n",
        toml_string(state.codest_src.as_deref().unwrap_or_default())
    ));
    for plugin in &state.plugins {
        out.push_str(&plugin.dependency_line());
        out.push('\n');
    }

    if state.needs_patch() {
        out.push_str(
            "\n# One contract crate for the whole graph. A plugin that names\n\
             # `cst-plugin-api` by version or git would otherwise resolve to a second\n\
             # copy, and `&SomePlugin` would not be a `&dyn Plugin`.\n\
             [patch.crates-io]\n",
        );
        out.push_str(&format!(
            "{CONTRACT_CRATE} = {{ path = {} }}\n",
            toml_string(&path_string(&contract_dir(state)))
        ));
    }
    out
}

/// The generated `main.rs` / `bin/cst.rs`.
fn render_binary(state: &InstallState, program: &str) -> String {
    let mut out = String::new();
    out.push_str("//! GENERATED by `codest plugin install` — do not edit.\n");
    out.push_str("//!\n");
    out.push_str("//! The plugin list lives in `~/.codest/plugins.toml`; re-run the\n");
    out.push_str("//! installer to change it.\n\n");
    out.push_str("use codest::plugins::Plugin;\n\n");
    if state.plugins.is_empty() {
        out.push_str("static LOCAL_PLUGINS: &[&dyn Plugin] = &[];\n\n");
    } else {
        out.push_str("static LOCAL_PLUGINS: &[&dyn Plugin] = &[\n");
        for plugin in &state.plugins {
            for symbol in &plugin.symbols {
                out.push_str(&format!("    &{}::{symbol},\n", plugin.crate_name));
            }
        }
        out.push_str("];\n\n");
    }
    out.push_str("fn main() {\n");
    out.push_str(&format!(
        "    codest::cli::run_blocking(\"{program}\", LOCAL_PLUGINS);\n"
    ));
    out.push_str("}\n");
    out
}

impl InstallState {
    /// The codest checkout the wrapper depends on.
    fn codest_src_dir(&self) -> PathBuf {
        self.codest_src
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_default()
    }
}

fn contract_dir(state: &InstallState) -> PathBuf {
    state.codest_src_dir().join(CONTRACT_SUBDIR)
}

/// Build the wrapper and return the debug/release `codest` binary.
fn build_wrapper(dir: &Path, release: bool) -> Result<PathBuf, InstallError> {
    let mut args = vec!["build", "--manifest-path"];
    let manifest = path_string(&dir.join("Cargo.toml"));
    args.push(&manifest);
    if release {
        args.push("--release");
    }
    run_cargo(&args, "build", dir)?;
    let profile = if release { "release" } else { "debug" };
    Ok(dir.join("target").join(profile).join(executable_name("codest")))
}

fn run_cargo(args: &[&str], operation: &'static str, dir: &Path) -> Result<(), InstallError> {
    eprintln!("codest: cargo {}", args.join(" "));
    let status = Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|source| InstallError::CargoMissing { source })?;
    if status.success() {
        return Ok(());
    }
    Err(InstallError::Build {
        operation,
        path: dir.to_path_buf(),
        status: status.to_string(),
    })
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Holds `$CODEST_HOME/plugins.lock` for the duration of a state change.
///
/// Two installs racing would interleave "read state, write state" and lose an
/// entry, so the whole read-modify-build sequence is exclusive.
struct StateLock {
    path: PathBuf,
}

impl StateLock {
    fn acquire(home: &Path) -> Result<Self, InstallError> {
        let path = home.join("plugins.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| InstallError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(InstallError::Locked { path })
            }
            Err(source) => Err(InstallError::Write { path, source }),
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn looks_like_path(target: &str) -> bool {
    target.starts_with('.')
        || target.starts_with('/')
        || target.starts_with('~')
        || target.contains(std::path::MAIN_SEPARATOR)
        || (cfg!(windows) && target.contains('/'))
}

fn looks_like_git_url(target: &str) -> bool {
    target.contains("://") || target.starts_with("git@")
}

/// `https://host/owner/name.git` → `name`.
fn package_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// A package name turned into an identifier usable in generated code.
pub fn crate_ident(package: &str) -> String {
    package.replace(['-', '.'], "_")
}

fn validate_crate_name(name: &str) -> Result<(), InstallError> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) => first.is_ascii_alphabetic() || first == '_',
        None => false,
    } && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if valid && is_not_keyword(name) {
        Ok(())
    } else {
        Err(InstallError::InvalidCrateName {
            name: name.to_string(),
        })
    }
}

/// Identifiers that cannot name a crate in `use`/path position.
fn is_not_keyword(name: &str) -> bool {
    !matches!(
        name,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
    )
}

/// Render a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Absolute and symlink-resolved. Falls back to the input when the path does
/// not exist; every caller has already reported a better error in that case.
///
/// A leading `~` is expanded, because `codest plugin install ~/plugins/x` is a
/// path the shell would ordinarily expand but which reaches us literally when
/// quoted.
fn canonical(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let path = expanded.as_path();
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|text| text.strip_prefix("~/")) else {
        return path.to_path_buf();
    };
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(rest))
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway home directory; removed on drop.
    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "codest-plugins-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&path).expect("temp home");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A fake codest checkout just complete enough to validate.
    fn fake_codest_root(home: &Path) -> PathBuf {
        let root = home.join("cst-src");
        std::fs::create_dir_all(root.join(CONTRACT_SUBDIR)).expect("contract dir");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"codest\"\nversion = \"0.1.0\"\n",
        )
        .expect("root manifest");
        std::fs::write(
            root.join(CONTRACT_SUBDIR).join("Cargo.toml"),
            "[package]\nname = \"cst-plugin-api\"\nversion = \"0.1.0\"\n",
        )
        .expect("contract manifest");
        root
    }

    /// A fake plugin checkout naming the contract by path.
    fn fake_plugin(root: &Path, name: &str, contract_pinned: bool) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        let contract = if contract_pinned {
            format!(
                "cst-plugin-api = {{ path = \"{}\" }}\n",
                root.join("cst-src").join(CONTRACT_SUBDIR).display()
            )
        } else {
            "cst-plugin-api = \"0.1\"\n".to_string()
        };
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{contract}"),
        )
        .expect("plugin manifest");
        dir
    }

    fn path_request(dir: &Path, codest_src: &Path) -> InstallRequest {
        InstallRequest {
            target: Some(dir.display().to_string()),
            codest_src: Some(codest_src.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn install_writes_state_and_a_wrapper_that_references_the_symbol() {
        let home = TempHome::new("install");
        let src = fake_codest_root(home.path());
        let plugin = fake_plugin(home.path(), "cst-provider-qoder", true);

        let outcome = install_in(home.path(), &path_request(&plugin, &src), false).expect("install");
        assert!(!outcome.replaced);
        assert_eq!(outcome.plugin.crate_name, "cst_provider_qoder");
        assert_eq!(outcome.plugin.symbols, vec!["PLUGIN".to_string()]);
        assert_eq!(outcome.plugin.kind(), SourceKind::Path);
        assert!(outcome.binary.is_none());

        let state = load_state(home.path()).expect("state");
        assert_eq!(state.plugins.len(), 1);
        // The recorded source is the canonical (symlink-resolved) checkout:
        // `/var` and `/private/var` are the same directory on macOS, but a
        // wrapper must not depend on which spelling the caller used.
        assert_eq!(
            state.codest_src.as_deref(),
            Some(path_string(&canonical(&src)).as_str())
        );

        let main = std::fs::read_to_string(crate_dir(home.path()).join("src/main.rs"))
            .expect("generated main");
        assert!(main.contains("&cst_provider_qoder::PLUGIN"));
        assert!(main.contains("codest::cli::run_blocking(\"codest\", LOCAL_PLUGINS)"));
        assert!(!main.contains("[patch"));
    }

    #[test]
    fn a_path_pinned_contract_needs_no_patch_and_a_version_pin_does() {
        let home = TempHome::new("patch");
        let src = fake_codest_root(home.path());
        let pinned = fake_plugin(home.path(), "pinned", true);
        let unpinned = fake_plugin(home.path(), "unpinned", false);

        install_in(home.path(), &path_request(&pinned, &src), false).expect("install pinned");
        let manifest =
            std::fs::read_to_string(crate_dir(home.path()).join("Cargo.toml")).expect("manifest");
        assert!(!manifest.contains("[patch.crates-io]"), "{manifest}");

        install_in(home.path(), &path_request(&unpinned, &src), false).expect("install unpinned");
        let manifest =
            std::fs::read_to_string(crate_dir(home.path()).join("Cargo.toml")).expect("manifest");
        assert!(manifest.contains("[patch.crates-io]"), "{manifest}");
        assert!(manifest.contains(CONTRACT_CRATE), "{manifest}");
    }

    #[test]
    fn installing_twice_replaces_the_entry_in_place() {
        let home = TempHome::new("replace");
        let src = fake_codest_root(home.path());
        let plugin = fake_plugin(home.path(), "tp", true);

        install_in(home.path(), &path_request(&plugin, &src), false).expect("first");
        let outcome = install_in(home.path(), &path_request(&plugin, &src), false).expect("second");
        assert!(outcome.replaced, "second install must report a replacement");
        assert_eq!(load_state(home.path()).expect("state").plugins.len(), 1);
    }

    #[test]
    fn uninstall_removes_the_entry_and_regenerates_a_plugin_free_binary() {
        let home = TempHome::new("uninstall");
        let src = fake_codest_root(home.path());
        let plugin = fake_plugin(home.path(), "tp", true);
        install_in(home.path(), &path_request(&plugin, &src), false).expect("install");

        let outcome = uninstall_in(home.path(), "tp", false).expect("uninstall");
        assert_eq!(outcome.remaining, 0);
        assert!(load_state(home.path()).expect("state").plugins.is_empty());
        let main = std::fs::read_to_string(crate_dir(home.path()).join("src/main.rs"))
            .expect("generated main");
        assert!(main.contains("static LOCAL_PLUGINS: &[&dyn Plugin] = &[]"), "{main}");
    }

    #[test]
    fn uninstalling_an_unknown_plugin_lists_what_is_there() {
        let home = TempHome::new("unknown");
        let error = uninstall_in(home.path(), "nope", false).expect_err("must fail");
        assert!(matches!(error, InstallError::NotInstalled { .. }));
        assert!(error.to_string().contains("none"));
    }

    #[test]
    fn a_directory_without_a_manifest_is_rejected() {
        let home = TempHome::new("not-a-plugin");
        let src = fake_codest_root(home.path());
        let empty = home.path().join("empty");
        std::fs::create_dir_all(&empty).expect("dir");
        let error = install_in(home.path(), &path_request(&empty, &src), false).expect_err("reject");
        assert!(matches!(error, InstallError::NotAPlugin { .. }), "{error}");
    }

    #[test]
    fn a_plugin_pointing_at_another_contract_copy_is_rejected() {
        let home = TempHome::new("mismatch");
        let src = fake_codest_root(home.path());
        let other = fake_codest_root(&home.path().join("elsewhere"));
        let dir = home.path().join("plugin");
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"plugin\"\nversion = \"0.1.0\"\n\n\
                 [dependencies]\ncst-plugin-api = {{ path = \"{}\" }}\n",
                other.join(CONTRACT_SUBDIR).display()
            ),
        )
        .expect("manifest");
        let error = install_in(home.path(), &path_request(&dir, &src), false).expect_err("reject");
        assert!(matches!(error, InstallError::ContractMismatch { .. }), "{error}");
    }

    #[test]
    fn git_and_registry_requests_do_not_need_a_checkout() {
        let git = resolve_plugin(
            &InstallRequest {
                target: Some("cst-provider-qoder".to_string()),
                git: Some("https://github.com/fufu/cst-provider-qoder".to_string()),
                tag: Some("v0.1.0".to_string()),
                ..Default::default()
            },
            Path::new("/nowhere"),
        )
        .expect("git request");
        assert_eq!(git.kind(), SourceKind::Git);
        assert_eq!(
            git.dependency_line(),
            "cst-provider-qoder = { git = \"https://github.com/fufu/cst-provider-qoder\", tag = \"v0.1.0\" }"
        );
        assert!(git.contract_pinned_path.is_none());

        let registry = resolve_plugin(
            &InstallRequest {
                target: Some("cst-provider-qoder".to_string()),
                ..Default::default()
            },
            Path::new("/nowhere"),
        )
        .expect("registry request");
        assert_eq!(registry.kind(), SourceKind::Registry);
        assert_eq!(
            registry.dependency_line(),
            "cst-provider-qoder = \"*\"".to_string()
        );
    }

    #[test]
    fn a_bare_git_url_infers_the_package_from_its_last_segment() {
        let plugin = resolve_plugin(
            &InstallRequest {
                target: Some("https://github.com/fufu/my-plugin.git".to_string()),
                ..Default::default()
            },
            Path::new("/nowhere"),
        )
        .expect("url request");
        assert_eq!(plugin.kind(), SourceKind::Git);
        assert_eq!(plugin.name, "my-plugin");
    }

    #[test]
    fn metadata_symbols_override_the_default() {
        let home = TempHome::new("symbols");
        let src = fake_codest_root(home.path());
        let dir = home.path().join("multi");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"multi\"\nversion = \"0.1.0\"\n\n\
             [package.metadata.codest]\nplugins = [\"A\", \"B\"]\n",
        )
        .expect("manifest");
        let outcome = install_in(home.path(), &path_request(&dir, &src), false).expect("install");
        assert_eq!(outcome.plugin.symbols, vec!["A".to_string(), "B".to_string()]);
        let main = std::fs::read_to_string(crate_dir(home.path()).join("src/main.rs")).expect("main");
        assert!(main.contains("&multi::A,"), "{main}");
        assert!(main.contains("&multi::B,"), "{main}");
    }

    #[test]
    fn crate_names_are_normalized_and_validated() {
        assert_eq!(crate_ident("cst-provider-qoder"), "cst_provider_qoder");
        assert!(validate_crate_name("cst_provider_qoder").is_ok());
        assert!(validate_crate_name("1plugin").is_err());
        assert!(validate_crate_name("has-dash").is_err());
        assert!(validate_crate_name("mod").is_err());
        assert!(validate_crate_name("").is_err());
    }

    #[test]
    fn a_leading_tilde_is_expanded() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("a home directory");
        assert_eq!(
            expand_tilde(Path::new("~/plugins/x")),
            PathBuf::from(home).join("plugins/x")
        );
        // Any other spelling is left alone.
        assert_eq!(expand_tilde(Path::new("~plugins")), PathBuf::from("~plugins"));
        assert_eq!(expand_tilde(Path::new("/abs/x")), PathBuf::from("/abs/x"));
    }

    #[test]
    fn state_round_trips_through_toml() {
        let state = InstallState {
            codest_src: Some("/tmp/cst".to_string()),
            plugins: vec![InstalledPlugin {
                name: "cst-provider-qoder".to_string(),
                crate_name: "cst_provider_qoder".to_string(),
                symbols: vec!["PLUGIN".to_string()],
                path: Some("/tmp/cst-provider-qoder".to_string()),
                git: None,
                rev: None,
                branch: None,
                tag: None,
                version: None,
                contract_pinned_path: Some("/tmp/cst/crates/cst-plugin-api".to_string()),
            }],
        };
        let text = toml::to_string_pretty(&state).expect("serialize");
        let parsed: InstallState = toml::from_str(&text).expect("parse");
        assert_eq!(parsed, state);
    }

    #[test]
    fn an_empty_state_needs_no_patch() {
        assert!(!InstallState::default().needs_patch());
    }

    #[test]
    fn an_explicit_codest_src_wins() {
        let home = TempHome::new("src");
        let src = fake_codest_root(home.path());
        let resolved =
            resolve_codest_src(Some(&src), Some("/somewhere/else")).expect("resolved");
        assert_eq!(resolved, canonical(&src));
    }

    #[test]
    fn a_wrong_explicit_codest_src_is_an_error() {
        let error = resolve_codest_src(Some(Path::new("/definitely/not/here")), None)
            .expect_err("must fail");
        assert!(matches!(error, InstallError::NotACodestSource { .. }), "{error}");
        assert!(error.to_string().contains("--codest-src"));
    }
}

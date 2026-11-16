//! Finding and layering configuration files.
//!
//! The load order is fixed: built-in defaults, the system file, every system
//! drop-in in name order, the user file, the active profile (including anything
//! it extends), the environment and finally explicit command-line overrides.
//! Everything is merged as TOML tables, so a profile only needs to name the
//! keys it changes.
//!
//! A version 1 document is migrated to version 2 before deserialisation: the
//! old single `policy.media.max_bitrate_kbps` becomes the audio cap, a video
//! cap is derived from the codec list, and `client.theme` stays the preset.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use toml::map::Entry;
use toml::{Table, Value};

use crate::config::{Config, DEFAULT_PROFILE, valid_profile_name};
use crate::error::{ConfigError, Result};

/// The deepest `extends` chain the loader accepts.
///
/// A chain this long is already pathological; the cap exists so a runaway or
/// hostile chain returns an error instead of exhausting the stack.
const MAX_PROFILE_DEPTH: usize = 32;

/// Where the system, user and profile files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub system: PathBuf,
    /// Directory of system drop-ins, merged in lexicographic file-name order.
    pub system_dropins: PathBuf,
    pub user: PathBuf,
    pub profiles: PathBuf,
}

impl ConfigPaths {
    /// Resolve the standard locations for this machine.
    ///
    /// `OVVC_CONFIG_DIR` wins over the platform configuration directory so a
    /// portable install or a test harness can keep everything in one place.
    pub fn discover() -> Result<Self> {
        let config_dir = std::env::var_os("OVVC_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("org", "ovvc", "ovvc")
                    .map(|project| project.config_dir().to_path_buf())
            })
            .ok_or(ConfigError::NoConfigDir)?;

        let system = std::env::var_os("OVVC_SYSTEM_CONFIG")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/ovvc/config.toml"));

        // `OVVC_SYSTEM_CONFIG_DIR` names the drop-in directory directly, which
        // is useful when the system file is redirected somewhere unusual.
        let system_dropins = std::env::var_os("OVVC_SYSTEM_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dropin_dir_for(&system));

        Ok(Self::with_roots_and_dropins(
            config_dir,
            system,
            system_dropins,
        ))
    }

    /// Build the paths for an explicit configuration directory and system
    /// file. Used by tests and by a portable install.
    ///
    /// The drop-in directory is derived from the system file's parent, so
    /// `/etc/ovvc/config.toml` gets `/etc/ovvc/config.d`.
    pub fn with_roots(config_dir: PathBuf, system: PathBuf) -> Self {
        let dropins = dropin_dir_for(&system);
        Self::with_roots_and_dropins(config_dir, system, dropins)
    }

    /// Build the paths for an explicit configuration directory, system file
    /// and drop-in directory. Used by tests that need to keep the drop-ins
    /// somewhere they control.
    pub fn with_roots_and_dropins(
        config_dir: PathBuf,
        system: PathBuf,
        system_dropins: PathBuf,
    ) -> Self {
        let profiles = config_dir.join("profiles");
        let user = config_dir.join("config.toml");
        Self {
            system,
            system_dropins,
            user,
            profiles,
        }
    }

    /// The file backing a named profile.
    pub fn profile(&self, name: &str) -> PathBuf {
        self.profiles.join(format!("{name}.toml"))
    }
}

/// The drop-in directory that sits beside a system configuration file.
fn dropin_dir_for(system: &Path) -> PathBuf {
    system
        .parent()
        .map(|parent| parent.join("config.d"))
        .unwrap_or_else(|| PathBuf::from("config.d"))
}

/// Values supplied on the command line, which beat every file and the
/// environment.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// An explicit configuration file used in place of the user file.
    pub config: Option<PathBuf>,
    /// The profile to apply.
    pub profile: Option<String>,
    /// Override the data directory.
    pub data_dir: Option<PathBuf>,
    /// Override the control socket path.
    pub socket: Option<PathBuf>,
    /// Override the client theme.
    pub theme: Option<String>,
}

/// Which kind of file a [`ConfigSource`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    System,
    /// One file from the system drop-in directory.
    SystemDropin,
    User,
    Profile,
    Explicit,
}

/// A file considered during loading, whether or not it was present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub layer: ConfigLayer,
    pub present: bool,
}

/// The fully merged configuration plus enough provenance to explain it.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub defaults: Config,
    pub sources: Vec<ConfigSource>,
    pub profile_chain: Vec<String>,
}

impl LoadedConfig {
    /// Flattened keys whose effective value differs from the built-in default.
    ///
    /// This is what `ovvc config diff` prints: only what the user changed, with
    /// the value that is actually in force after every layer.
    pub fn diff(&self) -> BTreeMap<String, String> {
        let defaults = flatten_config(&self.defaults);
        let effective = flatten_config(&self.config);
        effective
            .into_iter()
            .filter(|(key, value)| defaults.get(key) != Some(value))
            .collect()
    }
}

/// Load and validate the configuration.
pub fn load(paths: &ConfigPaths, overrides: Overrides) -> Result<LoadedConfig> {
    let defaults = Config::default();
    let mut merged =
        Value::try_from(&defaults).map_err(|error| ConfigError::Serialize(error.to_string()))?;
    let mut sources = Vec::new();

    // The system file is recorded even when absent so `config show --sources`
    // can say where ovvc looked.
    let system_present = config_file_present(&paths.system)?;
    sources.push(ConfigSource {
        path: paths.system.clone(),
        layer: ConfigLayer::System,
        present: system_present,
    });
    if system_present {
        let value = read_toml(&paths.system)?;
        merge(&mut merged, value);
    }

    // System drop-ins sit just above the system file and just below the user
    // file. Name order is the documented merge order, so an administrator can
    // number files and reason about the result without reading directory
    // timestamps.
    for path in system_dropin_files(&paths.system_dropins)? {
        sources.push(ConfigSource {
            path: path.clone(),
            layer: ConfigLayer::SystemDropin,
            present: true,
        });
        let value = read_toml(&path)?;
        merge(&mut merged, value);
    }

    // An explicit config path replaces the user file and must exist.
    if let Some(path) = &overrides.config {
        sources.push(ConfigSource {
            path: path.clone(),
            layer: ConfigLayer::Explicit,
            present: config_file_present(path)?,
        });
        let value = read_toml(path)?;
        merge(&mut merged, value);
    } else {
        let user_present = config_file_present(&paths.user)?;
        sources.push(ConfigSource {
            path: paths.user.clone(),
            layer: ConfigLayer::User,
            present: user_present,
        });
        if user_present {
            let value = read_toml(&paths.user)?;
            merge(&mut merged, value);
        }
    }

    // Choose the active profile before touching a profile file, so a name with
    // `..` or a separator can never escape the profiles directory.
    //
    // A `profile` key of the wrong type is a configuration error, not a reason
    // to fall back to `default`: silently replacing it would let a higher layer
    // hide the lower layer's choice.
    match merged.get("profile") {
        None | Some(Value::String(_)) => {}
        Some(_) => {
            return Err(ConfigError::Invalid(
                "profile: must be a string".to_string(),
            ));
        }
    }
    let active_profile = overrides
        .profile
        .clone()
        .or_else(|| {
            std::env::var("OVVC_PROFILE")
                .ok()
                .filter(|name| !name.is_empty())
        })
        .or_else(|| {
            merged
                .get("profile")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    if !valid_profile_name(&active_profile) {
        return Err(ConfigError::InvalidProfileName(active_profile));
    }

    let mut profile_chain = Vec::new();
    if active_profile == DEFAULT_PROFILE {
        profile_chain.push(DEFAULT_PROFILE.to_string());
    } else {
        let mut profile_values = Vec::new();
        let mut visiting = Vec::new();
        let mut seen = HashSet::new();
        resolve_profile(
            paths,
            &active_profile,
            &mut visiting,
            &mut seen,
            &mut profile_chain,
            &mut profile_values,
            &mut sources,
        )?;
        for value in profile_values {
            merge(&mut merged, value);
        }
    }
    set_key(
        &mut merged,
        &["profile"],
        Value::String(active_profile.clone()),
    );

    // Environment variables sit just below explicit flags. `OVVC_PROFILE` was
    // already consumed above. `OVVC_THEME` is deliberately not used: in
    // ovvc-theme it already names a theme file.
    if let Some(dir) = std::env::var_os("OVVC_DATA_DIR").filter(|value| !value.is_empty()) {
        set_key(&mut merged, &["data_dir"], Value::String(path_string(&dir)));
    }
    if let Some(socket) = std::env::var_os("OVVC_SOCKET").filter(|value| !value.is_empty()) {
        set_key(
            &mut merged,
            &["socket"],
            Value::String(path_string(&socket)),
        );
    }

    // Explicit flags beat everything.
    if let Some(dir) = &overrides.data_dir {
        set_key(
            &mut merged,
            &["data_dir"],
            Value::String(dir.to_string_lossy().into_owned()),
        );
    }
    if let Some(socket) = &overrides.socket {
        set_key(
            &mut merged,
            &["socket"],
            Value::String(socket.to_string_lossy().into_owned()),
        );
    }
    if let Some(theme) = &overrides.theme {
        set_key(
            &mut merged,
            &["client", "theme"],
            Value::String(theme.clone()),
        );
    }

    // Migrate the merged document before deserialising. A version 1 file is
    // rewritten in place so the rest of the loader only ever sees version 2.
    migrate(&mut merged)?;

    // Name every file that contributed, so an unknown key or a wrong type in a
    // drop-in is not blamed on a correct user file that happened to be the most
    // specific source. A single source still gets the exact-file error.
    let present: Vec<&ConfigSource> = sources.iter().filter(|source| source.present).collect();
    let config: Config = match merged.try_into() {
        Ok(config) => config,
        Err(source) => {
            if present.len() <= 1 {
                let path = present
                    .first()
                    .map(|source| source.path.clone())
                    .unwrap_or_else(|| paths.user.clone());
                return Err(ConfigError::Parse { path, source });
            }
            let sources = present
                .iter()
                .map(|source| source.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ConfigError::ParseMerged { sources, source });
        }
    };
    config.validate()?;

    Ok(LoadedConfig {
        config,
        defaults,
        sources,
        profile_chain,
    })
}

/// Write a default configuration file, refusing to replace one unless `force`
/// is set. The write goes through a temporary file in the same directory and
/// is then renamed, so a crash never leaves a half-written config.
pub fn init(path: &Path, force: bool) -> Result<PathBuf> {
    if path.exists() && !force {
        return Err(ConfigError::AlreadyExists {
            path: path.to_path_buf(),
        });
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let temp = temporary_path(path)?;
    fs::write(&temp, Config::default_toml()).map_err(|source| ConfigError::Io {
        path: temp.clone(),
        source,
    })?;
    fs::rename(&temp, path).map_err(|source| {
        let _ = fs::remove_file(&temp);
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(path.to_path_buf())
}

/// Load a profile and, depth first, every profile it extends.
///
/// `chain` ends up base first with the requested profile last, and `values`
/// holds the matching TOML documents with `extends` removed so they can be
/// merged in order.
fn resolve_profile(
    paths: &ConfigPaths,
    name: &str,
    visiting: &mut Vec<String>,
    seen: &mut HashSet<String>,
    chain: &mut Vec<String>,
    values: &mut Vec<Value>,
    sources: &mut Vec<ConfigSource>,
) -> Result<()> {
    if !valid_profile_name(name) {
        return Err(ConfigError::InvalidProfileName(name.to_string()));
    }
    if seen.contains(name) {
        let mut cycle = visiting.clone();
        cycle.push(name.to_string());
        return Err(ConfigError::ProfileCycle(cycle.join(" -> ")));
    }
    if visiting.len() >= MAX_PROFILE_DEPTH {
        let mut chain = visiting.clone();
        chain.push(name.to_string());
        return Err(ConfigError::ProfileTooDeep(
            MAX_PROFILE_DEPTH,
            chain.join(" -> "),
        ));
    }

    let path = paths.profile(name);
    if !config_file_present(&path)? {
        return Err(ConfigError::MissingProfile(name.to_string()));
    }

    let mut value = read_toml(&path)?;
    let parent = value
        .as_table_mut()
        .and_then(|table| table.remove("extends"))
        .map(|parent| match parent {
            Value::String(parent) => Ok(parent),
            _ => Err(ConfigError::Invalid(format!(
                "profiles.{name}.extends: must be a string"
            ))),
        })
        .transpose()?;

    visiting.push(name.to_string());
    seen.insert(name.to_string());
    if let Some(parent) = parent {
        resolve_profile(paths, &parent, visiting, seen, chain, values, sources)?;
    }
    visiting.pop();
    seen.remove(name);

    // Record the source after its parent, so `sources` follows the merge order
    // (base first, child last) and the reverse scan finds the most specific
    // file that contributed.
    sources.push(ConfigSource {
        path: path.clone(),
        layer: ConfigLayer::Profile,
        present: true,
    });

    chain.push(name.to_string());
    values.push(value);
    Ok(())
}

/// Whether a configuration file exists, following symlinks.
///
/// Only a genuinely absent path is treated as "no file". A permission error,
/// an unreadable parent directory or a dangling symlink is reported instead of
/// silently falling back to the built-in defaults. An existing path that is not
/// a regular file is also an error.
fn config_file_present(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_file() {
                Ok(true)
            } else {
                Err(ConfigError::Invalid(format!(
                    "{} exists but is not a regular file",
                    path.display()
                )))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // `metadata` follows symlinks, so a dangling link looks absent.
            // Distinguish the two with `symlink_metadata`: success means the
            // link itself exists and is broken.
            match fs::symlink_metadata(path) {
                Ok(_) => Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source: error,
                }),
                Err(link_error) if link_error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(link_error) => Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source: link_error,
                }),
            }
        }
        Err(error) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source: error,
        }),
    }
}

/// Read a TOML file, naming it in any error.
fn read_toml(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// List the `.toml` files in a drop-in directory, sorted lexicographically by
/// file name.
///
/// A missing directory is not an error, because most installs have no
/// drop-ins. Entries that are not regular files are skipped: `file_type` does
/// not follow symlinks, so a symlinked directory (or a symlinked file) is
/// ignored rather than followed out of the directory. Subdirectories are not
/// recursed into.
///
/// Public so `ovvc config validate` can name the drop-ins it considered before
/// a merged-document error is reported.
pub fn system_dropin_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ConfigError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| ConfigError::Io {
            path: entry.path(),
            source,
        })?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        files.push(path);
    }
    files.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(files)
}

/// Migrate a raw merged TOML document to the current configuration version.
///
/// This runs after every layer has merged and before deserialisation. The
/// built-in defaults already carry the version 2 keys, so a version 1 document
/// only needs its old media bitrate renamed and a video cap derived from
/// whichever codec list won the merge.
fn migrate(raw: &mut Value) -> Result<()> {
    // The built-in defaults already carry `version = 2`, so the merged
    // document's top-level version is 2 even when a user file omits it. Read
    // the declared version to reject a newer file, but decide whether this is
    // a version 1 document from the media keys too, otherwise a partial v1
    // file that names only `max_bitrate_kbps` never reaches the migration.
    match raw.get("version") {
        None | Some(Value::Integer(1)) | Some(Value::Integer(2)) => {}
        Some(Value::Integer(version)) if *version > 2 => {
            return Err(ConfigError::Invalid(format!(
                "version: {version} is newer than the supported version 2"
            )));
        }
        Some(Value::Integer(version)) => {
            return Err(ConfigError::Invalid(format!(
                "version: unsupported configuration version {version}"
            )));
        }
        Some(_) => {
            return Err(ConfigError::Invalid(
                "version: must be an integer".to_string(),
            ));
        }
    }

    let media_version = raw
        .get("policy")
        .and_then(|policy| policy.get("media"))
        .and_then(|media| media.get("version"))
        .and_then(Value::as_integer);
    if let Some(version) = media_version
        && version > 2
    {
        return Err(ConfigError::Invalid(format!(
            "policy.media.version: {version} is newer than the supported version 2"
        )));
    }
    let legacy_media = raw
        .get("policy")
        .and_then(|policy| policy.get("media"))
        .and_then(Value::as_table)
        .is_some_and(|media| media.contains_key("max_bitrate_kbps"));
    let is_v1 = matches!(raw.get("version"), Some(Value::Integer(1)))
        || media_version == Some(1)
        || legacy_media;
    if is_v1 {
        migrate_v1_media(raw);
    }
    set_key(raw, &["version"], Value::Integer(2));
    Ok(())
}

/// Rewrite the version 1 media bitrate into the split audio and video caps.
fn migrate_v1_media(raw: &mut Value) {
    let Some(media) = raw
        .get_mut("policy")
        .and_then(|policy| policy.get_mut("media"))
        .and_then(Value::as_table_mut)
    else {
        return;
    };

    // The old cap was a voice cap, so it becomes the audio cap. Version 1
    // allowed up to 20000 kbps while the split audio cap tops out at 512, so
    // clamp it rather than migrating a valid v1 file into an invalid v2 one.
    if let Some(Value::Integer(old)) = media.remove("max_bitrate_kbps") {
        media.insert(
            "max_audio_bitrate_kbps".to_string(),
            Value::Integer(old.min(512)),
        );
    }

    let has_video = media
        .get("codecs")
        .and_then(Value::as_array)
        .map(|codecs| {
            codecs
                .iter()
                .filter_map(Value::as_str)
                .any(|codec| matches!(codec, "vp8" | "vp9" | "av1" | "h264"))
        })
        .unwrap_or(false);
    media.insert(
        "max_video_bitrate_kbps".to_string(),
        Value::Integer(if has_video { 4000 } else { 0 }),
    );
    // A canonical v1 file carries `[policy.media] version = 1`. Bump it with
    // the top-level version, otherwise MediaPolicy::validate rejects the file
    // that `ovvc config init` itself wrote.
    media.insert(
        "version".to_string(),
        Value::Integer(ovvc_core::POLICY_VERSION as i64),
    );
}

/// Deep merge: tables recurse, every other value replaces.
fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.entry(key) {
                    Entry::Occupied(mut entry) => merge(entry.get_mut(), value),
                    Entry::Vacant(entry) => {
                        entry.insert(value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Set a dotted key, creating intermediate tables as needed.
fn set_key(root: &mut Value, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for key in parents {
        if !current.is_table() {
            *current = Value::Table(Table::new());
        }
        let table = current.as_table_mut().expect("table was just ensured");
        current = table
            .entry((*key).to_string())
            .or_insert_with(|| Value::Table(Table::new()));
    }
    if !current.is_table() {
        *current = Value::Table(Table::new());
    }
    current
        .as_table_mut()
        .expect("table was just ensured")
        .insert((*last).to_string(), value);
}

/// Render an OS string as a TOML string, replacing any non-UTF-8 bytes.
fn path_string(value: &std::ffi::OsStr) -> String {
    PathBuf::from(value).to_string_lossy().into_owned()
}

/// The temporary file `init` writes before renaming into place.
fn temporary_path(path: &Path) -> Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(ConfigError::Invalid(format!(
            "cannot write a configuration file at {}",
            path.display()
        )));
    };
    Ok(path.with_file_name(format!("{}.tmp", name.to_string_lossy())))
}

/// Flatten a configuration into dotted keys and rendered values.
fn flatten_config(config: &Config) -> BTreeMap<String, String> {
    let mut flat = BTreeMap::new();
    if let Ok(value) = Value::try_from(config) {
        flatten_value("", &value, &mut flat);
    }
    flat
}

fn flatten_value(prefix: &str, value: &Value, flat: &mut BTreeMap<String, String>) {
    match value {
        Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_value(&path, child, flat);
            }
        }
        other => {
            flat.insert(prefix.to_string(), render_value(other));
        }
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Datetime(datetime) => datetime.to_string(),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(render_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Table(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// `set_var` and `remove_var` mutate the process environment, so tests that
    /// use them must not run at the same time.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sets or removes an environment variable and restores it on drop.
    struct EnvVar {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    /// Clear every variable the loader reads so a developer's real environment
    /// cannot change a test result.
    fn clean_env() -> Vec<EnvVar> {
        [
            "OVVC_CONFIG_DIR",
            "OVVC_SYSTEM_CONFIG",
            "OVVC_SYSTEM_CONFIG_DIR",
            "OVVC_PROFILE",
            "OVVC_DATA_DIR",
            "OVVC_SOCKET",
        ]
        .into_iter()
        .map(EnvVar::remove)
        .collect()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        config_dir: PathBuf,
        system: PathBuf,
        dropins: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config_dir = dir.path().join("config");
            let system = dir.path().join("system.toml");
            let dropins = dir.path().join("dropins");
            fs::create_dir_all(&config_dir).unwrap();
            Self {
                _dir: dir,
                config_dir,
                system,
                dropins,
            }
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths::with_roots_and_dropins(
                self.config_dir.clone(),
                self.system.clone(),
                self.dropins.clone(),
            )
        }

        fn write_user(&self, text: &str) {
            fs::write(self.config_dir.join("config.toml"), text).unwrap();
        }

        fn write_system(&self, text: &str) {
            fs::write(&self.system, text).unwrap();
        }

        fn write_dropin(&self, name: &str, text: &str) {
            fs::create_dir_all(&self.dropins).unwrap();
            fs::write(self.dropins.join(name), text).unwrap();
        }

        fn write_profile(&self, name: &str, text: &str) {
            let profiles = self.config_dir.join("profiles");
            fs::create_dir_all(&profiles).unwrap();
            fs::write(profiles.join(format!("{name}.toml")), text).unwrap();
        }
    }

    fn load_default(paths: &ConfigPaths) -> LoadedConfig {
        load(paths, Overrides::default()).unwrap()
    }

    #[test]
    fn system_then_user_then_profile_precedence() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_system("[audio]\nframe_ms = 10\n");
        fixture.write_user("[audio]\nframe_ms = 20\n");
        fixture.write_profile("tuned", "[audio]\nframe_ms = 40\n");
        let paths = fixture.paths();

        let loaded = load(
            &paths,
            Overrides {
                profile: Some("tuned".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap();
        assert_eq!(loaded.config.audio.frame_ms, 40);
        assert_eq!(loaded.profile_chain, ["tuned"]);

        let loaded = load_default(&paths);
        assert_eq!(loaded.config.audio.frame_ms, 20);

        fs::remove_file(fixture.config_dir.join("config.toml")).unwrap();
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.audio.frame_ms, 10);
    }

    #[test]
    fn absent_sources_are_recorded() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let loaded = load_default(&fixture.paths());

        let system = loaded
            .sources
            .iter()
            .find(|source| source.layer == ConfigLayer::System)
            .unwrap();
        assert!(!system.present);
        let user = loaded
            .sources
            .iter()
            .find(|source| source.layer == ConfigLayer::User)
            .unwrap();
        assert!(!user.present);
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn overrides_and_environment_beat_files() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user(
            "data_dir = \"/from/file\"\n\
             socket = \"/from/file.sock\"\n\
             [client]\n\
             theme = \"file\"\n",
        );
        let paths = fixture.paths();
        let _data_dir = EnvVar::set("OVVC_DATA_DIR", "/from/env");
        let _socket = EnvVar::set("OVVC_SOCKET", "/from/env.sock");

        let loaded = load(
            &paths,
            Overrides {
                data_dir: Some(PathBuf::from("/from/cli")),
                theme: Some("cli".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap();
        assert_eq!(loaded.config.data_dir, Some(PathBuf::from("/from/cli")));
        assert_eq!(loaded.config.socket, Some(PathBuf::from("/from/env.sock")));
        assert_eq!(loaded.config.client.theme.as_deref(), Some("cli"));

        let loaded = load_default(&paths);
        assert_eq!(loaded.config.data_dir, Some(PathBuf::from("/from/env")));
        assert_eq!(loaded.config.client.theme.as_deref(), Some("file"));
    }

    #[test]
    fn environment_profile_beats_the_file_and_flags_beat_both() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user("profile = \"default\"\n");
        fixture.write_profile("envprofile", "[audio]\nframe_ms = 10\n");
        fixture.write_profile("cli", "[audio]\nframe_ms = 40\n");
        let paths = fixture.paths();
        let _profile = EnvVar::set("OVVC_PROFILE", "envprofile");

        let loaded = load_default(&paths);
        assert_eq!(loaded.config.profile, "envprofile");
        assert_eq!(loaded.config.audio.frame_ms, 10);

        let loaded = load(
            &paths,
            Overrides {
                profile: Some("cli".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap();
        assert_eq!(loaded.config.profile, "cli");
        assert_eq!(loaded.config.audio.frame_ms, 40);
    }

    #[test]
    fn a_non_string_profile_is_an_error_instead_of_default() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_system("profile = \"work\"\n");
        fixture.write_profile("work", "[audio]\nframe_ms = 10\n");
        fixture.write_user("profile = 0\n");

        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => assert!(message.contains("profile"), "{message}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn an_extends_chain_deeper_than_the_cap_is_an_error_not_a_stack_overflow() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let depth = MAX_PROFILE_DEPTH + 4;
        for index in 0..depth {
            let name = format!("p{index:03}");
            let text = if index + 1 < depth {
                format!("extends = \"p{:03}\"\n", index + 1)
            } else {
                "[audio]\nframe_ms = 20\n".to_string()
            };
            fixture.write_profile(&name, &text);
        }

        let error = load(
            &fixture.paths(),
            Overrides {
                profile: Some("p000".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match error {
            ConfigError::ProfileTooDeep(limit, chain) => {
                assert_eq!(limit, MAX_PROFILE_DEPTH);
                assert!(chain.contains("p000"), "{chain}");
            }
            other => panic!("expected ProfileTooDeep, got {other:?}"),
        }
    }

    #[test]
    fn a_parse_error_names_the_child_profile_that_set_the_key() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_profile("base", "[audio]\nframe_ms = 20\n");
        fixture.write_profile("child", "extends = \"base\"\n[audio]\nframe_size = 20\n");

        let error = load(
            &fixture.paths(),
            Overrides {
                profile: Some("child".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match error {
            ConfigError::ParseMerged { sources, .. } => {
                assert!(
                    sources.contains("child.toml"),
                    "the child that set the key must be named: {sources}"
                );
                assert!(
                    sources.contains("base.toml"),
                    "the merged sources are all named: {sources}"
                );
            }
            other => panic!("expected ParseMerged naming the profiles, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlinked_config_is_reported_not_ignored() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let link = fixture.config_dir.join("config.toml");
        std::os::unix::fs::symlink(fixture.config_dir.join("absent.toml"), &link).unwrap();

        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::Io { path, .. } => assert_eq!(path, link),
            other => panic!("expected Io for a dangling symlink, got {other:?}"),
        }
    }

    #[test]
    fn a_profile_extends_a_base_and_the_child_wins() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_profile(
            "base",
            "[client]\ntheme = \"base\"\n[audio]\nframe_ms = 10\n",
        );
        fixture.write_profile("child", "extends = \"base\"\n[audio]\nframe_ms = 40\n");
        let paths = fixture.paths();

        let loaded = load(
            &paths,
            Overrides {
                profile: Some("child".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap();
        assert_eq!(loaded.profile_chain, ["base", "child"]);
        assert_eq!(loaded.config.audio.frame_ms, 40);
        assert_eq!(loaded.config.client.theme.as_deref(), Some("base"));
        assert!(loaded.profile_chain.len() == 2);
        let profile_sources: Vec<_> = loaded
            .sources
            .iter()
            .filter(|source| source.layer == ConfigLayer::Profile)
            .collect();
        assert_eq!(profile_sources.len(), 2);
    }

    #[test]
    fn a_two_profile_cycle_is_reported_with_its_chain() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_profile("a", "extends = \"b\"\n");
        fixture.write_profile("b", "extends = \"a\"\n");
        let paths = fixture.paths();

        let error = load(
            &paths,
            Overrides {
                profile: Some("a".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match error {
            ConfigError::ProfileCycle(chain) => {
                assert!(chain.contains('a'), "{chain}");
                assert!(chain.contains('b'), "{chain}");
            }
            other => panic!("expected ProfileCycle, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_non_default_profile_is_an_error() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let paths = fixture.paths();

        let error = load(
            &paths,
            Overrides {
                profile: Some("does-not-exist".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match error {
            ConfigError::MissingProfile(name) => assert_eq!(name, "does-not-exist"),
            other => panic!("expected MissingProfile, got {other:?}"),
        }
    }

    #[test]
    fn unknown_keys_are_parse_errors_and_bad_values_are_invalid() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write_user("[audio]\nframe_size = 20\n");
        let error = load(&paths, Overrides::default()).unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");

        fixture.write_user("[audio]\nframe_ms = 25\n");
        let error = load(&paths, Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => assert!(message.contains("audio.frame_ms")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_dropin_key_error_names_every_contributing_source() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user("[audio]\nframe_ms = 20\n");
        fixture.write_dropin("50-bad.toml", "[audio]\nframe_size = 20\n");

        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::ParseMerged { sources, source } => {
                assert!(
                    sources.contains("50-bad.toml"),
                    "the offending drop-in must be named: {sources}"
                );
                assert!(
                    sources.contains("config.toml"),
                    "every contributor is named rather than blaming one: {sources}"
                );
                assert!(source.to_string().contains("frame_size"), "{source}");
            }
            other => panic!("expected ParseMerged, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_config_file_must_exist() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let missing = fixture.config_dir.join("missing.toml");

        let error = load(
            &fixture.paths(),
            Overrides {
                config: Some(missing.clone()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match error {
            ConfigError::Io { path, .. } => assert_eq!(path, missing),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn traversal_profile_names_are_rejected_before_any_file_access() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let paths = fixture.paths();

        for name in ["..", "../escape", "nested/profile"] {
            let error = load(
                &paths,
                Overrides {
                    profile: Some(name.to_string()),
                    ..Overrides::default()
                },
            )
            .unwrap_err();
            match error {
                ConfigError::InvalidProfileName(reported) => assert_eq!(reported, name),
                other => panic!("expected InvalidProfileName for {name:?}, got {other:?}"),
            }
        }

        // The same rule applies to a profile's `extends` target.
        fixture.write_profile("child", "extends = \"../escape\"\n");
        let error = load(
            &paths,
            Overrides {
                profile: Some("child".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProfileName(_)),
            "{error:?}"
        );
    }

    #[test]
    fn diff_lists_only_overridden_keys_and_their_effective_values() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user("[audio]\nframe_ms = 40\n[client]\ntheme = \"dark\"\n");
        fixture.write_profile("quiet", "[client]\ntheme = \"quiet\"\n");
        let paths = fixture.paths();

        let loaded = load(
            &paths,
            Overrides {
                profile: Some("quiet".to_string()),
                ..Overrides::default()
            },
        )
        .unwrap();
        let diff = loaded.diff();

        assert_eq!(diff.get("audio.frame_ms").map(String::as_str), Some("40"));
        assert_eq!(diff.get("client.theme").map(String::as_str), Some("quiet"));
        assert_eq!(diff.get("profile").map(String::as_str), Some("quiet"));
        assert!(!diff.contains_key("audio.engine"));
        assert!(!diff.contains_key("video.codecs"));
        assert_eq!(diff.len(), 3);
    }

    #[test]
    fn init_writes_a_loadable_file_and_refuses_to_overwrite() {
        let _lock = env_lock();
        let _env = clean_env();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("config.toml");

        let written = init(&target, false).unwrap();
        assert_eq!(written, target);
        assert!(target.is_file());

        let paths =
            ConfigPaths::with_roots(dir.path().join("nested"), dir.path().join("system.toml"));
        let loaded = load_default(&paths);
        assert_eq!(loaded.config, Config::default());

        let error = init(&target, false).unwrap_err();
        assert!(
            matches!(error, ConfigError::AlreadyExists { .. }),
            "{error:?}"
        );

        assert_eq!(init(&target, true).unwrap(), target);
    }

    #[test]
    fn config_paths_profile_joins_under_the_profiles_directory() {
        let paths = ConfigPaths::with_roots(
            PathBuf::from("/tmp/ovvc-test"),
            PathBuf::from("/etc/ovvc/config.toml"),
        );
        assert_eq!(
            paths.profile("work"),
            PathBuf::from("/tmp/ovvc-test/profiles/work.toml")
        );
        assert_eq!(paths.user, PathBuf::from("/tmp/ovvc-test/config.toml"));
        assert_eq!(paths.system, PathBuf::from("/etc/ovvc/config.toml"));
        assert_eq!(
            paths.system_dropins,
            PathBuf::from("/etc/ovvc/config.d"),
            "the drop-in directory is derived from the system file's parent"
        );

        let explicit = ConfigPaths::with_roots_and_dropins(
            PathBuf::from("/tmp/ovvc-test"),
            PathBuf::from("/etc/ovvc/config.toml"),
            PathBuf::from("/tmp/ovvc-dropins"),
        );
        assert_eq!(explicit.system_dropins, PathBuf::from("/tmp/ovvc-dropins"));
    }

    #[test]
    fn discover_honours_the_environment() {
        let _lock = env_lock();
        let _env = clean_env();
        let _config_dir = EnvVar::set("OVVC_CONFIG_DIR", "/tmp/ovvc-discover");
        let _system = EnvVar::set("OVVC_SYSTEM_CONFIG", "/tmp/ovvc-system.toml");

        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.user, PathBuf::from("/tmp/ovvc-discover/config.toml"));
        assert_eq!(paths.profiles, PathBuf::from("/tmp/ovvc-discover/profiles"));
        assert_eq!(paths.system, PathBuf::from("/tmp/ovvc-system.toml"));
        assert_eq!(
            paths.system_dropins,
            PathBuf::from("/tmp/config.d"),
            "without an override the drop-ins sit beside the system file"
        );

        let _dropins = EnvVar::set("OVVC_SYSTEM_CONFIG_DIR", "/tmp/ovvc-dropins");
        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.system_dropins, PathBuf::from("/tmp/ovvc-dropins"));
    }

    #[test]
    fn a_star_hub_policy_reaches_the_loaded_config() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user(
            "version = 2\n\
             [policy.media]\n\
             call_scope = \"star\"\n\
             [policy.media.hub]\n\
             mode = \"elected\"\n\
             trust = \"ciphertext-only\"\n\
             allow_mixing = false\n\
             allow_video_forwarding = true\n\
             min_trust_tier = 1\n",
        );
        let loaded = load_default(&fixture.paths());
        assert_eq!(
            loaded.config.policy.media.call_scope,
            ovvc_core::CallScope::Star
        );
        let hub = loaded.config.policy.media.hub;
        assert_eq!(hub.mode, ovvc_core::HubMode::Elected);
        assert_eq!(hub.trust, ovvc_core::HubTrust::CiphertextOnly);
        assert!(!hub.allow_mixing);
        assert!(hub.allow_video_forwarding);
        assert_eq!(hub.min_trust_tier, 1);
        assert!(!hub.can_decrypt());
        assert!(!hub.can_mix());
    }

    #[test]
    fn a_version_one_document_migrates_to_split_media_caps() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write_user(
            "version = 1\n\
             [policy.media]\n\
             codecs = [\"opus\"]\n\
             max_bitrate_kbps = 128\n",
        );
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.version, 2);
        assert_eq!(loaded.config.policy.media.version, 2);
        assert_eq!(loaded.config.policy.media.max_audio_bitrate_kbps, 128);
        assert_eq!(loaded.config.policy.media.max_video_bitrate_kbps, 0);
        assert!(!loaded.config.policy.media.video_enabled());

        // With a video codec the old cap still becomes the audio cap, and the
        // documented standard video cap is filled in.
        fixture.write_user(
            "version = 1\n\
             [policy.media]\n\
             codecs = [\"opus\", \"vp9\"]\n\
             max_bitrate_kbps = 256\n",
        );
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.version, 2);
        assert_eq!(loaded.config.policy.media.max_audio_bitrate_kbps, 256);
        assert_eq!(loaded.config.policy.media.max_video_bitrate_kbps, 4000);
        assert!(loaded.config.policy.media.video_enabled());

        // A partial v1 file that names only the old key and no `version`: the
        // defaults contribute `version = 2`, which used to hide it from the
        // migration and then fail as an unknown field.
        fixture.write_user("[policy.media]\nmax_bitrate_kbps = 128\n");
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.version, 2);
        assert_eq!(loaded.config.policy.media.max_audio_bitrate_kbps, 128);

        // A canonical v1 file as `ovvc config init` wrote it: both the
        // top-level and the media version are 1, and the media version must be
        // bumped or MediaPolicy::validate refuses it.
        fixture.write_user(
            "version = 1\n\
             profile = \"default\"\n\
             \n\
             [daemon]\nnotify = false\ntray = false\nonline = false\n\
             [audio]\nengine = \"opus\"\nbackend = \"auto\"\nsample_rate = 48000\nframe_ms = 20\nprofile = \"voice-standard\"\nptt_only = false\nplugins = []\n\
             [policy.media]\nversion = 1\ne2ee = true\ntransport = \"direct\"\ntopology = \"mesh\"\ncodecs = [\"opus\", \"vp9\"]\nmax_bitrate_kbps = 128\nallow_video_over_tor = false\n",
        );
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.version, 2);
        assert_eq!(loaded.config.policy.media.version, 2);
        assert_eq!(loaded.config.policy.media.max_audio_bitrate_kbps, 128);
        assert_eq!(loaded.config.policy.media.max_video_bitrate_kbps, 4000);

        // Version 1 allowed up to 20000 kbps; the split audio cap stops at
        // 512, so a valid v1 file must not migrate into an invalid v2 one.
        fixture.write_user("version = 1\n[policy.media]\nmax_bitrate_kbps = 1080\n");
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.version, 2);
        assert_eq!(
            loaded.config.policy.media.max_audio_bitrate_kbps, 512,
            "the migrated audio cap is clamped to the v2 ceiling"
        );
    }

    #[test]
    fn a_newer_configuration_version_is_rejected_with_the_version() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user("version = 3\n");

        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => {
                assert!(message.contains("version"), "{message}");
                assert!(message.contains('3'), "{message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn system_dropins_merge_in_name_order_and_the_user_file_wins() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_dropin("10-low.toml", "[audio]\nframe_ms = 10\n");
        fixture.write_dropin("20-high.toml", "[audio]\nframe_ms = 40\n");
        fixture.write_user("[audio]\nframe_ms = 20\n");
        let paths = fixture.paths();

        let loaded = load_default(&paths);
        assert_eq!(
            loaded.config.audio.frame_ms, 20,
            "the user file beats every drop-in"
        );

        fs::remove_file(fixture.config_dir.join("config.toml")).unwrap();
        let loaded = load_default(&paths);
        assert_eq!(
            loaded.config.audio.frame_ms, 40,
            "later file names win within the drop-in layer"
        );

        let dropins: Vec<_> = loaded
            .sources
            .iter()
            .filter(|source| source.layer == ConfigLayer::SystemDropin)
            .collect();
        assert_eq!(dropins.len(), 2);
        assert!(dropins.iter().all(|source| source.present));
        assert!(dropins[0].path.ends_with("10-low.toml"));
        assert!(dropins[1].path.ends_with("20-high.toml"));
    }

    #[test]
    fn a_missing_dropin_directory_is_not_an_error() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        assert!(!fixture.dropins.exists());

        let loaded = load_default(&fixture.paths());
        assert_eq!(loaded.config, Config::default());
        assert!(
            !loaded
                .sources
                .iter()
                .any(|source| source.layer == ConfigLayer::SystemDropin)
        );
    }

    #[test]
    fn a_dropin_entry_that_is_not_a_toml_file_is_ignored() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_dropin("README", "[audio]\nframe_ms = 40\n");
        fixture.write_dropin("notes.txt", "[audio]\nframe_ms = 40\n");
        fs::create_dir_all(fixture.dropins.join("nested")).unwrap();
        fs::write(
            fixture.dropins.join("nested").join("inner.toml"),
            "[audio]\nframe_ms = 40\n",
        )
        .unwrap();

        let loaded = load_default(&fixture.paths());
        assert_eq!(loaded.config.audio.frame_ms, 20);
        assert!(
            !loaded
                .sources
                .iter()
                .any(|source| source.layer == ConfigLayer::SystemDropin)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_dropin_entry_is_not_followed() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let outside = fixture._dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        // A version 3 file would fail the load if the symlinked directory were
        // followed, so a successful load proves it was skipped.
        fs::write(outside.join("newer.toml"), "version = 3\n").unwrap();
        fs::create_dir_all(&fixture.dropins).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.dropins.join("linked")).unwrap();

        let loaded = load_default(&fixture.paths());
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn theme_file_validates_and_appears_in_the_diff() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write_user(
            "[client]\n\
             theme = \"phosphor\"\n\
             theme_file = \"/tmp/custom-theme.toml\"\n",
        );
        let loaded = load_default(&paths);
        assert_eq!(loaded.config.client.theme.as_deref(), Some("phosphor"));
        assert_eq!(
            loaded.config.client.theme_file,
            Some(PathBuf::from("/tmp/custom-theme.toml"))
        );
        let diff = loaded.diff();
        assert_eq!(
            diff.get("client.theme").map(String::as_str),
            Some("phosphor")
        );
        assert_eq!(
            diff.get("client.theme_file").map(String::as_str),
            Some("/tmp/custom-theme.toml")
        );

        fixture.write_user("[client]\ntheme_file = \"\"\n");
        let error = load(&paths, Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => assert!(message.contains("client.theme_file")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_version_two_file_without_media_gets_the_defaults_without_a_version_bump() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user("version = 2\n[audio]\nframe_ms = 20\n");

        let loaded = load_default(&fixture.paths());
        assert_eq!(
            loaded.config.version, 2,
            "an additive section is not a bump"
        );
        assert_eq!(loaded.config.media, crate::config::MediaConfig::default());
        assert_eq!(loaded.config.media.transport, "webrtc");
        assert!(loaded.config.media.ice_servers.is_empty());
        assert!(!loaded.config.media.force_relay);

        // Nothing was set in [media], so nothing there is reported as changed.
        let diff = loaded.diff();
        assert!(
            !diff.keys().any(|key| key.starts_with("media.")),
            "{diff:?}"
        );
    }

    #[test]
    fn media_overrides_reach_the_diff_and_are_validated() {
        let _lock = env_lock();
        let _env = clean_env();
        let fixture = Fixture::new();
        fixture.write_user(
            "[media]\n\
             transport = \"libp2p\"\n\
             ice_servers = [\"stun:stun.example.org:3478\"]\n",
        );
        let loaded = load_default(&fixture.paths());
        assert_eq!(loaded.config.media.transport, "libp2p");
        let diff = loaded.diff();
        assert_eq!(
            diff.get("media.transport").map(String::as_str),
            Some("libp2p")
        );
        assert_eq!(
            diff.get("media.ice_servers").map(String::as_str),
            Some("[stun:stun.example.org:3478]")
        );

        fixture.write_user("[media]\ntransport = \"carrier-pigeon\"\n");
        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => assert!(message.contains("media.transport")),
            other => panic!("expected Invalid, got {other:?}"),
        }

        fixture.write_user("[media]\nforce_relay = true\n");
        let error = load(&fixture.paths(), Overrides::default()).unwrap_err();
        match error {
            ConfigError::Invalid(message) => assert!(message.contains("force_relay")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// A JET country site. Only NL is wired up for now, but everything that differs per
/// country lives here so adding e.g. lieferando.de is a table entry.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub code: &'static str,
    pub site_host: &'static str,
    /// Brand-specific auth host; Turnstile on the login page only works on this domain.
    pub auth_host: &'static str,
    pub language: &'static str,
}

pub const TENANTS: &[Tenant] = &[Tenant {
    code: "nl",
    site_host: "www.thuisbezorgd.nl",
    auth_host: "auth.thuisbezorgd.nl",
    language: "nl-NL",
}];

impl Tenant {
    pub fn auth_base(&self) -> String {
        format!("https://{}", self.auth_host)
    }
}

pub fn tenant(code: &str) -> Result<Tenant> {
    TENANTS
        .iter()
        .find(|t| t.code == code)
        .cloned()
        .with_context(|| format!("unsupported country '{code}' (supported: nl)"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Location {
    pub label: String,
    pub postcode: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub tenant: String,
    /// Email used for the last login; offered as the default next time.
    pub email: Option<String>,
    /// Chromium-based browser used for `jet login`; auto-detected when unset.
    pub browser: Option<String>,
    pub location: Option<Location>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tenant: "nl".into(),
            email: None,
            browser: None,
            location: None,
        }
    }
}

#[derive(Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "jet-cli").context("cannot determine home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn auth_file(&self) -> PathBuf {
        self.config_dir.join("auth.json")
    }
    pub fn basket_file(&self) -> PathBuf {
        self.config_dir.join("basket.json")
    }
    pub fn last_list_file(&self) -> PathBuf {
        self.config_dir.join("last-restaurants.json")
    }
    pub fn last_orders_file(&self) -> PathBuf {
        self.config_dir.join("last-orders.json")
    }
    pub fn browser_profile(&self) -> PathBuf {
        self.data_dir.join("browser-profile")
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(paths.config_file()) {
            Ok(s) => toml::from_str(&s).context("invalid config.toml"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_private(
            &paths.config_file(),
            toml::to_string_pretty(self)?.as_bytes(),
        )
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).with_context(|| format!("invalid {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, serde_json::to_string_pretty(value)?.as_bytes())
}

/// Writes atomically with 0600 permissions; these files hold tokens.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

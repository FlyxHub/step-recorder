//! User settings, saved as JSON (on Windows: %APPDATA%\Step Recorder\settings.json).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Obsidian vault folders the user can send recordings to.
    #[serde(default)]
    pub vaults: Vec<PathBuf>,
    /// The vault picked last time, preselected in the review window.
    #[serde(default)]
    pub last_vault: Option<PathBuf>,
}

impl Settings {
    /// Missing or unreadable file → defaults.
    // ponytail: a corrupt file is silently replaced on next save; add a backup if that bites.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)
    }

    /// Adds a vault unless it's already listed. Returns its index either way.
    pub fn add_vault(&mut self, vault: PathBuf) -> usize {
        match self.vaults.iter().position(|v| *v == vault) {
            Some(i) => i,
            None => {
                self.vaults.push(vault);
                self.vaults.len() - 1
            }
        }
    }

    pub fn remove_vault(&mut self, index: usize) {
        if index < self.vaults.len() {
            let removed = self.vaults.remove(index);
            if self.last_vault.as_ref() == Some(&removed) {
                self.last_vault = None;
            }
        }
    }

    /// Index of `last_vault` in the list, else the first vault.
    pub fn preferred(&self) -> Option<usize> {
        let last = self.last_vault.as_ref().and_then(|l| self.vaults.iter().position(|v| v == l));
        last.or(if self.vaults.is_empty() { None } else { Some(0) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_and_preferred() {
        let mut s = Settings::default();
        assert_eq!(s.preferred(), None);
        assert_eq!(s.add_vault("C:/A".into()), 0);
        assert_eq!(s.add_vault("C:/B".into()), 1);
        assert_eq!(s.add_vault("C:/A".into()), 0); // no duplicate
        assert_eq!(s.vaults.len(), 2);
        assert_eq!(s.preferred(), Some(0));
        s.last_vault = Some("C:/B".into());
        assert_eq!(s.preferred(), Some(1));
        s.remove_vault(1);
        assert_eq!(s.last_vault, None);
        assert_eq!(s.preferred(), Some(0));
        s.remove_vault(7); // out of range: no-op
        assert_eq!(s.vaults, [PathBuf::from("C:/A")]);
    }

    #[test]
    fn save_load_round_trip_and_missing_file() {
        let dir = std::env::temp_dir().join(format!("step-recorder-test-{}", std::process::id()));
        let path = dir.join("nested").join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let s = Settings { vaults: vec!["C:/Vault".into()], last_vault: Some("C:/Vault".into()) };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        fs::remove_dir_all(&dir).unwrap();
    }
}

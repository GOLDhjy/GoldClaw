use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use goldclaw_config::ProjectPaths;

#[derive(Clone, Debug)]
pub struct StorePaths {
    pub database_file: PathBuf,
    pub backup_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct StoreLayout {
    paths: StorePaths,
}

impl StoreLayout {
    pub fn from_project_paths(project_paths: &ProjectPaths) -> Self {
        Self {
            paths: StorePaths {
                database_file: project_paths.database_file(),
                backup_dir: project_paths.backup_dir(),
            },
        }
    }

    pub fn from_paths(database_file: PathBuf, backup_dir: PathBuf) -> Self {
        Self {
            paths: StorePaths {
                database_file,
                backup_dir,
            },
        }
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn ensure_parent_dirs(&self) -> std::io::Result<()> {
        if let Some(parent) = self.paths.database_file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.paths.backup_dir)?;
        Ok(())
    }

    pub fn backup_path(&self, timestamp: SystemTime) -> PathBuf {
        let seconds = timestamp
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.paths
            .backup_dir
            .join(format!("goldclaw-{seconds}.sqlite3.bak"))
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod layout_tests;

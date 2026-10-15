//! Opt-in installation of Pi's local timing observer. Never edits native auth,
//! models or settings; never replaces/deletes a user-modified extension.

use crate::config::atomic_write_private;
use crate::error::AppError;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

const SOURCE: &str = include_str!("../../../integrations/pi/cc-switch-metrics.ts");
const FILE_NAME: &str = "cc-switch-metrics.ts";
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiObservationStatus {
    /// Installed on disk, not a claim that an already-running Pi reloaded it.
    enabled: bool,
    can_manage: bool,
    extension_path: String,
}

pub(crate) fn status() -> Result<PiObservationStatus, AppError> {
    status_at(&crate::pi_config::get_pi_agent_dir()?)
}

fn status_at(agent_dir: &Path) -> Result<PiObservationStatus, AppError> {
    let extensions = agent_dir.join("extensions");
    let path = extensions.join(FILE_NAME);
    let safe_parent = match fs::symlink_metadata(&extensions) {
        Ok(metadata) => metadata.file_type().is_dir(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(AppError::io(&extensions, error)),
    };
    let (enabled, can_manage) = match fs::symlink_metadata(&path) {
        Ok(metadata) if safe_parent && metadata.file_type().is_file() => {
            let owned = metadata.len() == SOURCE.len() as u64
                && fs::read(&path).map_err(|error| AppError::io(&path, error))?
                    == SOURCE.as_bytes();
            (owned, owned)
        }
        Ok(_) => (false, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, safe_parent),
        Err(error) => return Err(AppError::io(&path, error)),
    };
    Ok(PiObservationStatus {
        enabled,
        can_manage,
        extension_path: path.to_string_lossy().into_owned(),
    })
}

pub(crate) fn set_enabled(enabled: bool) -> Result<PiObservationStatus, AppError> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| AppError::Config("Pi 观测扩展安装锁已损坏".into()))?;
    set_enabled_at(&crate::pi_config::get_pi_agent_dir()?, enabled)
}

fn set_enabled_at(agent_dir: &Path, enabled: bool) -> Result<PiObservationStatus, AppError> {
    let current = status_at(agent_dir)?;
    if !current.can_manage {
        return Err(AppError::Config(format!(
            "Pi 观测扩展路径已被其他文件占用或修改，不会覆盖: {}",
            current.extension_path
        )));
    }
    let path = agent_dir.join("extensions").join(FILE_NAME);
    if enabled && !current.enabled {
        atomic_write_private(&path, SOURCE.as_bytes())?;
    } else if !enabled && current.enabled {
        fs::remove_file(&path).map_err(|error| AppError::io(&path, error))?;
    }
    status_at(agent_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_disable_are_idempotent_and_leave_native_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["auth.json", "models.json", "settings.json"] {
            fs::write(dir.path().join(file), "untouched").unwrap();
        }
        assert!(!status_at(dir.path()).unwrap().enabled);
        assert!(set_enabled_at(dir.path(), true).unwrap().enabled);
        assert!(set_enabled_at(dir.path(), true).unwrap().enabled);
        assert!(!set_enabled_at(dir.path(), false).unwrap().enabled);
        assert!(!set_enabled_at(dir.path(), false).unwrap().enabled);
        for file in ["auth.json", "models.json", "settings.json"] {
            assert_eq!(
                fs::read_to_string(dir.path().join(file)).unwrap(),
                "untouched"
            );
        }
    }

    #[test]
    fn never_overwrites_or_removes_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        set_enabled_at(dir.path(), true).unwrap();
        let path = dir.path().join("extensions").join(FILE_NAME);
        fs::write(&path, "my extension").unwrap();
        assert!(!status_at(dir.path()).unwrap().can_manage);
        assert!(set_enabled_at(dir.path(), true).is_err());
        assert!(set_enabled_at(dir.path(), false).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "my extension");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_extension_file_or_directory() {
        let dir = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(external.path(), dir.path().join("extensions")).unwrap();
        assert!(set_enabled_at(dir.path(), true).is_err());
        fs::remove_file(dir.path().join("extensions")).unwrap();
        fs::create_dir(dir.path().join("extensions")).unwrap();
        let target = external.path().join("user.ts");
        fs::write(&target, SOURCE).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("extensions").join(FILE_NAME)).unwrap();
        assert!(set_enabled_at(dir.path(), false).is_err());
        assert!(target.exists());
    }
}

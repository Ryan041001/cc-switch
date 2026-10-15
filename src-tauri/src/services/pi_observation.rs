//! Opt-in installation of Pi's local timing observer. Never edits native auth,
//! models or settings; never replaces/deletes a user-modified extension.

use crate::config::atomic_write_private;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

const SOURCE: &str = include_str!("../../../integrations/pi/cc-switch-metrics.ts");
const FILE_NAME: &str = "cc-switch-metrics.ts";
// Only exact, released versions are ours to upgrade. A marker/comment is not proof.
const LEGACY_LEN: u64 = 3674;
const LEGACY_SHA256: &str = "ddc9989bd698b63d54fcf2e57352c54d928030bb9017d1be6c5c79542bcd9808";
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiObservationStatus {
    /// Installed on disk, not a claim that an already-running Pi reloaded it.
    enabled: bool,
    can_manage: bool,
    update_available: bool,
    extension_path: String,
    /// Most recent imported client observation, not a runtime heartbeat.
    last_observed_at: Option<i64>,
}

pub(crate) fn status(db: &Database) -> Result<PiObservationStatus, AppError> {
    with_last_observation(db, status_at(&crate::pi_config::get_pi_agent_dir()?)?)
}

fn with_last_observation(
    db: &Database,
    mut status: PiObservationStatus,
) -> Result<PiObservationStatus, AppError> {
    status.last_observed_at = lock_conn!(db.conn)
        .query_row(
            "SELECT MAX(created_at) FROM proxy_request_logs
         WHERE app_type = 'pi' AND data_source IN
         ('pi_observed', 'pi_observed_call', 'pi_observed_turn')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Database(format!("读取 Pi 实测状态失败: {error}")))?;
    Ok(status)
}

fn status_at(agent_dir: &Path) -> Result<PiObservationStatus, AppError> {
    let extensions = agent_dir.join("extensions");
    let path = extensions.join(FILE_NAME);
    let safe_parent = match fs::symlink_metadata(&extensions) {
        Ok(metadata) => metadata.file_type().is_dir(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(AppError::io(&extensions, error)),
    };
    let (enabled, can_manage, update_available) = match fs::symlink_metadata(&path) {
        Ok(metadata) if safe_parent && metadata.file_type().is_file() => {
            // Bound reads of an untrusted file at this path.
            let candidate = metadata.len() == SOURCE.len() as u64 || metadata.len() == LEGACY_LEN;
            let bytes = if candidate {
                fs::read(&path).map_err(|error| AppError::io(&path, error))?
            } else {
                Vec::new()
            };
            let current = bytes == SOURCE.as_bytes();
            let legacy = metadata.len() == LEGACY_LEN
                && format!("{:x}", Sha256::digest(&bytes)) == LEGACY_SHA256;
            (current || legacy, current || legacy, legacy)
        }
        Ok(_) => (false, false, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, safe_parent, false),
        Err(error) => return Err(AppError::io(&path, error)),
    };
    Ok(PiObservationStatus {
        enabled,
        can_manage,
        update_available,
        extension_path: path.to_string_lossy().into_owned(),
        last_observed_at: None,
    })
}

pub(crate) fn set_enabled(db: &Database, enabled: bool) -> Result<PiObservationStatus, AppError> {
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| AppError::Config("Pi 观测扩展安装锁已损坏".into()))?;
    with_last_observation(
        db,
        set_enabled_at(&crate::pi_config::get_pi_agent_dir()?, enabled)?,
    )
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
    if enabled && (!current.enabled || current.update_available) {
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
    fn upgrades_only_the_exact_previous_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extensions").join(FILE_NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = include_str!("../../../tests/fixtures/pi/cc-switch-metrics-v1.ts.txt");
        fs::write(&path, legacy).unwrap();
        let status = status_at(dir.path()).unwrap();
        assert!(status.enabled && status.can_manage && status.update_available);
        let updated = set_enabled_at(dir.path(), true).unwrap();
        assert!(updated.enabled && !updated.update_available);
        assert_eq!(fs::read_to_string(&path).unwrap(), SOURCE);
        // A same-length modification must not pass the ownership check.
        fs::write(&path, legacy.replacen("local-only", "local-edit", 1)).unwrap();
        assert!(!status_at(dir.path()).unwrap().can_manage);
        assert!(set_enabled_at(dir.path(), true).is_err());
        assert!(set_enabled_at(dir.path(), false).is_err());
        fs::write(&path, legacy).unwrap();
        assert!(!set_enabled_at(dir.path(), false).unwrap().enabled);
    }

    #[test]
    fn last_observation_is_imported_client_data_not_installation_or_native_usage(
    ) -> Result<(), AppError> {
        let db = Database::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let initial =
            with_last_observation(&db, set_enabled_at(dir.path(), true).unwrap()).unwrap();
        assert!(initial.enabled);
        assert_eq!(initial.last_observed_at, None);
        {
            let conn = lock_conn!(db.conn);
            for (id, source, at) in [
                ("client", "pi_observed_call", 100),
                ("history", "pi_session", 200),
                ("native", "antigravity_native", 300),
            ] {
                conn.execute(
                    "INSERT INTO proxy_request_logs
                     (request_id, provider_id, app_type, model, latency_ms, status_code, created_at, data_source)
                     VALUES (?1, 'provider', 'pi', 'model', 1000, 200, ?2, ?3)",
                    rusqlite::params![id, at, source],
                ).unwrap();
            }
        }
        let status = with_last_observation(&db, status_at(dir.path()).unwrap()).unwrap();
        assert_eq!(status.last_observed_at, Some(100));
        Ok(())
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

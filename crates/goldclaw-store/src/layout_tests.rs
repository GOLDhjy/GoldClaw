use super::*;
use std::time::Duration;

#[test]
fn backup_path_is_stable() {
    let layout = StoreLayout {
        paths: StorePaths {
            database_file: PathBuf::from("db/goldclaw.sqlite3"),
            backup_dir: PathBuf::from("backups"),
        },
    };

    let backup = layout.backup_path(UNIX_EPOCH + Duration::from_secs(42));
    assert_eq!(backup, PathBuf::from("backups/goldclaw-42.sqlite3.bak"));
}

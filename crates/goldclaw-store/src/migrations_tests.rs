use super::*;

#[test]
fn schema_version_tracks_latest_migration() {
    assert_eq!(current_schema_version(), 3);
}

#[test]
fn migrations_are_sorted() {
    assert!(
        MIGRATIONS
            .windows(2)
            .all(|pair| pair[0].version < pair[1].version)
    );
}

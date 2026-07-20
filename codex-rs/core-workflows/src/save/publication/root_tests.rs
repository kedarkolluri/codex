use super::private_unix_directory_attributes_are_safe;

#[test]
fn private_directory_attributes_require_effective_user_ownership() {
    assert!(private_unix_directory_attributes_are_safe(
        1000, 0o040755, 1000
    ));
    assert!(!private_unix_directory_attributes_are_safe(
        1001, 0o040700, 1000
    ));
}

#[test]
fn private_directory_attributes_reject_group_or_other_write() {
    assert!(private_unix_directory_attributes_are_safe(
        1000, 0o040755, 1000
    ));
    assert!(!private_unix_directory_attributes_are_safe(
        1000, 0o040775, 1000
    ));
    assert!(!private_unix_directory_attributes_are_safe(
        1000, 0o040757, 1000
    ));
}

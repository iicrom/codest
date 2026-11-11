//! Session identity.
//!
//! One id answers "which session is this" everywhere: it is the rollout file
//! name, the value in `SessionMeta.id`, the key the worker and the TUI address
//! sessions by, and (when the provider asks for one) the session header on
//! model requests.
//!
//! Ids are UUIDv7 rather than codex's opaque counter or the old hex-nanosecond
//! timestamp. v7 is time-ordered, so lexicographic file-name order is creation
//! order — the property `/sessions` and `--continue` depend on — while staying
//! a standard, copy-pastable string (`codex resume <uuid>` in spirit).

use uuid::Uuid;

/// Mint the id for a new session.
///
/// The id is allocated **eagerly**, when the session is created, not when its
/// rollout file is first written: a turn needs a session header before the
/// file exists. See [`crate::core::session`] for the lazy-file half.
pub fn new_session_id() -> String {
    Uuid::now_v7().to_string()
}

/// Whether `value` is a session id this build can read back from disk.
///
/// Anything else — including the pre-UUID hex-nanosecond file names — is not a
/// session this build knows about.
pub fn is_session_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_uuid_v7_and_sort_by_creation() {
        let first = new_session_id();
        let second = new_session_id();

        assert!(is_session_id(&first), "minted ids must parse back: {first}");
        assert!(is_session_id(&second));
        assert_eq!(Uuid::parse_str(&first).unwrap().get_version_num(), 7);
        // v7's timestamp prefix makes lexical order creation order.
        assert!(first < second, "{first} should sort before {second}");
    }

    #[test]
    fn legacy_timestamp_names_are_not_session_ids() {
        // The pre-UUID file name was `format!("{:x}", now_nanos())`.
        assert!(!is_session_id("18f2c3d4e5f60718"));
        assert!(!is_session_id(""));
        assert!(!is_session_id("not-a-uuid"));
    }
}

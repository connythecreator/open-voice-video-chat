//! Errors raised by the bundled redb-backed store.
//!
//! [`StoreError`] exists so the store can use `?` on redb's error types while
//! the [`Storage`](ovvc_core::Storage) trait stays backend-agnostic. Anything
//! that is not a domain error is preserved as a message in
//! [`CoreError::Backend`].

use ovvc_core::error::CoreError;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Postcard(#[from] postcard::Error),

    #[error("database error: {0}")]
    Database(#[from] redb::DatabaseError),

    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
}

pub type Result<T> = std::result::Result<T, StoreError>;

impl From<StoreError> for CoreError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Core(error) => error,
            StoreError::Io(error) => CoreError::Io(error),
            StoreError::Postcard(error) => CoreError::Postcard(error),
            other => CoreError::Backend(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_and_io_errors_pass_through_with_their_kind() {
        let core: CoreError = StoreError::Core(CoreError::NotFound("guild".to_string())).into();
        assert!(matches!(core, CoreError::NotFound(ref m) if m == "guild"));

        let io: CoreError = StoreError::Io(std::io::Error::other("disk")).into();
        assert!(matches!(io, CoreError::Io(_)));

        let postcard: CoreError = StoreError::Postcard(postcard::Error::DeserializeUnexpectedEnd).into();
        assert!(matches!(postcard, CoreError::Postcard(_)));
    }

    #[test]
    fn backend_errors_become_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twice.redb");
        let _first = redb::Database::create(&path).unwrap();
        let second = redb::Database::create(&path).unwrap_err();
        let error: CoreError = StoreError::Database(second).into();
        match error {
            CoreError::Backend(message) => assert!(!message.is_empty()),
            other => panic!("expected a backend error, got {other:?}"),
        }
    }

    #[test]
    fn display_names_the_failure_class() {
        assert!(StoreError::Io(std::io::Error::other("disk"))
            .to_string()
            .contains("io error"));
        assert!(StoreError::Postcard(postcard::Error::DeserializeUnexpectedEnd)
            .to_string()
            .contains("serialization error"));
    }
}

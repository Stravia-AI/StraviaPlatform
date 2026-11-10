//! Operation-scoped atomic state. A cell handle is released automatically on
//! drop; it must not be retained beyond the operation that opened it.
//!
//! Load, compute a new value without side effects, then compare and exchange.
//! On [`Error::Conflict`], reload and recompute rather than repeating external
//! requests. Deletion is a compare-and-exchange to a tombstone, not a reset of
//! the revision to zero.

mod bindings {
    wit_bindgen::generate!({
        world: "state-imports",
        path: "wit",
        generate_all,
    });
}

use bindings::stravia::state::{store, volatile};

/// Maximum cell payload size in bytes, shared by durable, conversation,
/// process-volatile and journal snapshots.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Host-selected scope; no provider or conversation identifier is supplied by
/// the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Provider,
    Conversation,
}

/// Revision zero denotes a cell that has never existed. A deleted cell has no
/// payload but retains its nonzero revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub payload: Option<Vec<u8>>,
}

/// A revision conflict is not a host failure and can be retried after reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Conflict,
    Host { code: String, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => f.write_str("atomic state revision conflict"),
            Self::Host { code, message } => write!(f, "atomic state {code}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<store::Error> for Error {
    fn from(error: store::Error) -> Self {
        Self::Host {
            code: error.code,
            message: error.message,
        }
    }
}

impl From<Error> for crate::PluginError {
    fn from(error: Error) -> Self {
        let (kind, message) = match error {
            Error::Conflict => (
                crate::ErrorKind::Trapped,
                "atomic state revision conflict".to_owned(),
            ),
            Error::Host { code, message } => {
                let kind = match code.as_str() {
                    "unsupported" => crate::ErrorKind::Unsupported,
                    "invalid" => crate::ErrorKind::Invalid,
                    "cancelled" => crate::ErrorKind::Cancelled,
                    "deadline-exceeded" => crate::ErrorKind::DeadlineExceeded,
                    "resource-exhausted" => crate::ErrorKind::ResourceExhausted,
                    "provider-not-found" => crate::ErrorKind::ProviderNotFound,
                    // State failures are local host failures, not malformed upstream responses.
                    _ => crate::ErrorKind::Trapped,
                };
                (kind, message)
            }
        };
        crate::PluginError {
            kind,
            message,
            upstream_status: None,
        }
    }
}

/// An operation-bound host resource, released when dropped.
pub struct Cell(store::Cell);

/// Open a named cell in the scope bound by the host to this operation. Names
/// are limited to 128 UTF-8 bytes and each scope supports at most 16 keys.
pub fn open(scope: Scope, key: &str) -> Result<Cell, Error> {
    let scope = match scope {
        Scope::Provider => store::Scope::Provider,
        Scope::Conversation => store::Scope::Conversation,
    };
    store::open(scope, key).map(Cell).map_err(Into::into)
}

/// Open process-volatile provider state selected by the host. It shares the
/// ordinary cell limits and CAS semantics, but is never persisted to disk.
pub fn open_volatile(key: &str) -> Result<Cell, Error> {
    volatile::open(key).map(Cell).map_err(Into::into)
}

impl Cell {
    pub fn load(&self) -> Result<Snapshot, Error> {
        self.0
            .load()
            .map(|snapshot| Snapshot {
                revision: snapshot.revision,
                payload: snapshot.payload,
            })
            .map_err(Into::into)
    }

    /// Set or delete a payload if the current revision matches. Payloads are
    /// limited to 1 MiB (1,048,576 bytes). A conflict does not change the cell.
    pub fn compare_exchange(
        &self,
        expected_revision: u64,
        payload: Option<&[u8]>,
    ) -> Result<(), Error> {
        match self.0.compare_exchange(expected_revision, payload) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::Conflict),
            Err(error) => Err(error.into()),
        }
    }

    /// Delete while preserving a fresh revision tombstone.
    pub fn delete(&self, expected_revision: u64) -> Result<(), Error> {
        self.compare_exchange(expected_revision, None)
    }
}

/// Immutable journal entries and atomic cell/head snapshots.
///
/// Journal handles are operation-bound host resources and are released on drop.
/// Journals are available only for conversation-scoped cells.
pub mod journal {
    use super::{Cell, Error, bindings};
    use bindings::stravia::state::journal as wit;

    /// A host-owned immutable journal node.
    pub struct Entry(wit::Entry);

    /// The document and previous node of an entry.
    pub struct EntryData {
        pub text: String,
        pub previous: Option<Entry>,
    }

    /// A revision, payload and journal head read atomically.
    pub struct Snapshot {
        pub revision: u64,
        pub payload: Option<Vec<u8>>,
        pub head: Option<Entry>,
    }

    impl Entry {
        /// Read this document and acquire a handle to its previous node.
        pub fn read(&self) -> Result<EntryData, Error> {
            self.0
                .read()
                .map(|data| EntryData {
                    text: data.text,
                    previous: data.previous.map(Entry),
                })
                .map_err(Into::into)
        }
    }

    /// Load payload, revision and head from the same cell snapshot.
    pub fn load(cell: &Cell) -> Result<Snapshot, Error> {
        wit::load(&cell.0)
            .map(|snapshot| Snapshot {
                revision: snapshot.revision,
                payload: snapshot.payload,
                head: snapshot.head.map(Entry),
            })
            .map_err(Into::into)
    }

    /// Stage a document whose previous node belongs to this operation.
    pub fn append(cell: &Cell, text: &str, previous: Option<&Entry>) -> Result<Entry, Error> {
        wit::append(&cell.0, text, previous.map(|entry| &entry.0))
            .map(Entry)
            .map_err(Into::into)
    }

    /// Atomically replace the payload and head if the revision matches.
    /// Payloads use the same 1 MiB limit as ordinary cells.
    pub fn compare_exchange(
        cell: &Cell,
        expected_revision: u64,
        payload: Option<&[u8]>,
        head: Option<&Entry>,
    ) -> Result<(), Error> {
        match wit::compare_exchange(
            &cell.0,
            expected_revision,
            payload,
            head.map(|entry| &entry.0),
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::Conflict),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_errors_preserve_code_and_message() {
        let error = Error::from(store::Error {
            code: "future-code".into(),
            message: "safe message".into(),
        });
        assert_eq!(
            error,
            Error::Host {
                code: "future-code".into(),
                message: "safe message".into(),
            }
        );
    }

    #[test]
    fn unknown_host_failure_is_not_an_upstream_parse_error() {
        let error: crate::PluginError = Error::Host {
            code: "future-code".into(),
            message: "local failure".into(),
        }
        .into();
        assert!(matches!(error.kind, crate::ErrorKind::Trapped));
        assert_eq!(error.message, "local failure");
        assert_eq!(error.upstream_status, None);
    }

    #[test]
    fn host_failure_maps_to_plugin_error() {
        let error: crate::PluginError = Error::Host {
            code: "resource-exhausted".into(),
            message: "state limit reached".into(),
        }
        .into();
        assert!(matches!(error.kind, crate::ErrorKind::ResourceExhausted));
        assert_eq!(error.message, "state limit reached");
    }
}

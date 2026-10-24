//! The `[new folder]` verb's model: a unique untitled name, a fail-closed
//! preflight, and the single filesystem call that creates the directory.
//!
//! Pure on the naming and planning side (no I/O until `execute`), so the
//! header can decide, the boundary can revalidate, and tests can pin both
//! without a temp directory. TP-FM-NEWDIR-01/02/03.

use std::path::{Path, PathBuf};

use super::rename::{validate_rename_name_component, RenameNameIssue, RenameNamePlatform};

/// Finder's name for a folder nobody has named yet.
pub(crate) const UNTITLED_FOLDER_NAME: &str = "untitled folder";

/// `untitled folder`, then `untitled folder 2`, `untitled folder 3`, … — the
/// first name not already taken in the listing. Byte-exact comparison: on the
/// filesystems Herdr runs on, `Untitled folder` and `untitled folder` are two
/// different entries, so the second must not be mistaken for the first.
pub(crate) fn unique_untitled_name<'a>(existing: impl IntoIterator<Item = &'a str>) -> String {
    let taken = existing
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    if !taken.contains(UNTITLED_FOLDER_NAME) {
        return UNTITLED_FOLDER_NAME.to_string();
    }
    (2u32..)
        .map(|n| format!("{UNTITLED_FOLDER_NAME} {n}"))
        .find(|candidate| !taken.contains(candidate.as_str()))
        .unwrap_or_else(|| UNTITLED_FOLDER_NAME.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateDirectoryRequest {
    pub parent: PathBuf,
    pub name: String,
    pub operation_in_flight: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateDirectoryPreflightError {
    OperationInFlight,
    InvalidName(RenameNameIssue),
    AlreadyExists,
    ParentNotDirectory,
}

/// A validated request: the name is a legal single component, the parent is a
/// directory, and nothing of that name exists there yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateDirectoryPlan {
    parent: PathBuf,
    name: String,
    destination: PathBuf,
}

impl CreateDirectoryPlan {
    pub(crate) fn preflight(
        request: CreateDirectoryRequest,
    ) -> Result<Self, CreateDirectoryPreflightError> {
        if request.operation_in_flight {
            return Err(CreateDirectoryPreflightError::OperationInFlight);
        }
        validate_rename_name_component(&request.name, RenameNamePlatform::current())
            .map_err(CreateDirectoryPreflightError::InvalidName)?;
        if !request.parent.is_dir() {
            return Err(CreateDirectoryPreflightError::ParentNotDirectory);
        }
        let destination = request.parent.join(&request.name);
        // `symlink_metadata`, not `exists`: a dangling symlink of that name is
        // still a name in use, and `mkdir` would fail on it either way.
        if destination.symlink_metadata().is_ok() {
            return Err(CreateDirectoryPreflightError::AlreadyExists);
        }
        Ok(Self {
            parent: request.parent,
            name: request.name,
            destination,
        })
    }

    pub(crate) fn parent(&self) -> &Path {
        &self.parent
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateDirectoryError {
    AlreadyExists,
    PermissionDenied,
    ParentMissing,
    Io(String),
}

/// The one filesystem effect: `mkdir`, not `mkdir -p` — a parent that vanished
/// between preflight and execution is reported, not silently recreated.
pub(crate) fn execute_create_directory(
    plan: &CreateDirectoryPlan,
) -> Result<PathBuf, CreateDirectoryError> {
    match std::fs::create_dir(plan.destination()) {
        Ok(()) => Ok(plan.destination().to_path_buf()),
        Err(err) => Err(match err.kind() {
            std::io::ErrorKind::AlreadyExists => CreateDirectoryError::AlreadyExists,
            std::io::ErrorKind::PermissionDenied => CreateDirectoryError::PermissionDenied,
            std::io::ErrorKind::NotFound => CreateDirectoryError::ParentMissing,
            _ => CreateDirectoryError::Io(err.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-create-dir-test-{}-{}-{}",
                std::process::id(),
                tag,
                unique()
            ));
            fs::create_dir_all(&root).expect("create temp root");
            Self { root }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn request(parent: &Path, name: &str) -> CreateDirectoryRequest {
        CreateDirectoryRequest {
            parent: parent.to_path_buf(),
            name: name.to_string(),
            operation_in_flight: false,
        }
    }

    // TA2.1 — TP-FM-NEWDIR-01: Finder's counting, from the live listing.
    #[test]
    fn untitled_name_counts_up_from_what_the_listing_holds() {
        assert_eq!(unique_untitled_name([]), "untitled folder");
        assert_eq!(
            unique_untitled_name(["untitled folder"]),
            "untitled folder 2"
        );
        assert_eq!(
            unique_untitled_name(["untitled folder", "untitled folder 2"]),
            "untitled folder 3"
        );
        // A gap is filled, not skipped: 2 is free when only 3 exists.
        assert_eq!(
            unique_untitled_name(["untitled folder", "untitled folder 3"]),
            "untitled folder 2"
        );
        // Unrelated names do not count, and case is a different name here.
        assert_eq!(
            unique_untitled_name(["Untitled folder", "notes.md", "untitled folder 10"]),
            "untitled folder"
        );
    }

    // TA2.2 — TP-FM-NEWDIR-02/03: the preflight refuses before touching disk.
    #[test]
    fn preflight_refuses_bad_names_busy_operations_and_taken_names() {
        let td = TempDir::new("preflight");
        for bad in ["", ".", "..", "a/b", "nul\0byte"] {
            let error = CreateDirectoryPlan::preflight(request(&td.root, bad))
                .expect_err("a bad name is refused");
            assert!(
                matches!(error, CreateDirectoryPreflightError::InvalidName(_)),
                "{bad:?} → {error:?}"
            );
        }
        let busy = CreateDirectoryRequest {
            operation_in_flight: true,
            ..request(&td.root, "untitled folder")
        };
        assert_eq!(
            CreateDirectoryPlan::preflight(busy),
            Err(CreateDirectoryPreflightError::OperationInFlight)
        );
        fs::create_dir(td.root.join("taken")).expect("fixture dir");
        assert_eq!(
            CreateDirectoryPlan::preflight(request(&td.root, "taken")),
            Err(CreateDirectoryPreflightError::AlreadyExists)
        );
        // A FILE of that name is just as taken as a directory.
        fs::write(td.root.join("taken.txt"), b"x").expect("fixture file");
        assert_eq!(
            CreateDirectoryPlan::preflight(request(&td.root, "taken.txt")),
            Err(CreateDirectoryPreflightError::AlreadyExists)
        );
        let file_parent = td.root.join("taken.txt");
        assert_eq!(
            CreateDirectoryPlan::preflight(request(&file_parent, "child")),
            Err(CreateDirectoryPreflightError::ParentNotDirectory)
        );
        assert!(
            fs::read_dir(&td.root).expect("fixture listing").count() == 2,
            "preflight created nothing"
        );
    }

    // TA2.2 — a good request plans the exact destination.
    #[test]
    fn preflight_plans_the_destination_under_the_parent() {
        let td = TempDir::new("plan");
        let plan = CreateDirectoryPlan::preflight(request(&td.root, "untitled folder"))
            .expect("a free name plans");
        assert_eq!(plan.parent(), td.root.as_path());
        assert_eq!(plan.name(), "untitled folder");
        assert_eq!(
            plan.destination(),
            td.root.join("untitled folder").as_path()
        );
        assert!(!plan.destination().exists(), "planning creates nothing");
    }

    // TA2.5/TA2.7 — TP-FM-NEWDIR-03: execution creates exactly one directory,
    // and a parent that vanished is a named failure, not a silent mkdir -p.
    #[test]
    fn execute_creates_the_directory_once_and_names_a_missing_parent() {
        let td = TempDir::new("execute");
        let plan =
            CreateDirectoryPlan::preflight(request(&td.root, "untitled folder")).expect("plan");
        let created = execute_create_directory(&plan).expect("mkdir succeeds");
        assert_eq!(created, td.root.join("untitled folder"));
        assert!(created.is_dir());
        assert_eq!(
            execute_create_directory(&plan),
            Err(CreateDirectoryError::AlreadyExists),
            "the second execution of the same plan does not pretend"
        );

        let gone = td.root.join("vanishes");
        fs::create_dir(&gone).expect("fixture parent");
        let plan = CreateDirectoryPlan::preflight(request(&gone, "child")).expect("plan");
        fs::remove_dir(&gone).expect("parent vanishes between preflight and execution");
        assert_eq!(
            execute_create_directory(&plan),
            Err(CreateDirectoryError::ParentMissing)
        );
        assert!(!gone.exists(), "no parent was recreated");
    }
}

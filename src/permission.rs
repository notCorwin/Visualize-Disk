use std::{fs::File, io::ErrorKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Granted,
    Denied,
    Unverified,
}

const SENTINEL: &str = "/Library/Application Support/com.apple.TCC/TCC.db";

/// Verify access for the running process by actually opening a fixed,
/// TCC-protected database read-only. No contents are read or private APIs used.
/// A missing database, ordinary UNIX denial or unexpected error fails closed.
/// Never derive this path from HOME or other caller-controlled environment.
pub fn check() -> Access {
    if !cfg!(target_os = "macos") {
        return Access::Unverified;
    }
    probe(|path| File::open(path).map(drop))
}

fn probe(open: impl FnOnce(&std::path::Path) -> std::io::Result<()>) -> Access {
    match open(std::path::Path::new(SENTINEL)) {
        Ok(()) => Access::Granted,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => Access::Denied,
        Err(_) => Access::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denied_and_missing_are_never_granted() {
        assert_eq!(
            probe(|_| Err(ErrorKind::PermissionDenied.into())),
            Access::Denied
        );
        assert_eq!(
            probe(|_| Err(ErrorKind::NotFound.into())),
            Access::Unverified
        );
        assert_eq!(probe(|_| Err(ErrorKind::Other.into())), Access::Unverified);
    }
    #[test]
    fn only_actual_open_of_fixed_system_database_grants_access() {
        assert_eq!(
            probe(|path| {
                assert_eq!(
                    path,
                    std::path::Path::new("/Library/Application Support/com.apple.TCC/TCC.db")
                );
                assert!(!path.starts_with(std::env::temp_dir()));
                Ok(())
            }),
            Access::Granted
        );
    }
}

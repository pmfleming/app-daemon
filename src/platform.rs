use std::{env, os::unix::fs::PermissionsExt, path::Path};

/// Returns whether a command can be executed directly or found on `PATH`.
pub(crate) fn command_available(command: &str) -> bool {
    let command = Path::new(command);
    if command.components().count() > 1 {
        return executable(command);
    }
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| executable(&dir.join(command))))
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

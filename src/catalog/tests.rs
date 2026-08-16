use std::fs;

use super::Catalog;

#[test]
fn identifies_terminal_applications_and_parses_their_commands() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("terminal.desktop"),
        "[Desktop Entry]\nType=Application\nName=Terminal app\nExec=btop --utf-force\nTerminal=true\n",
    )?;

    let catalog = Catalog::from_paths(vec![directory.path().into()]);
    let entry = &catalog.entries[0];
    assert!(entry.requires_terminal());
    assert_eq!(entry.launch_command()?, ["btop", "--utf-force"]);
    Ok(())
}

#[test]
fn revision_tracks_all_visible_catalog_metadata() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.desktop");
    fs::write(
        &path,
        "[Desktop Entry]\nType=Application\nName=Example\nComment=Before\nExec=true\n",
    )?;
    let before = Catalog::from_paths(vec![directory.path().into()]).revision;
    fs::write(
        path,
        "[Desktop Entry]\nType=Application\nName=Example\nComment=After\nExec=true\n",
    )?;
    let after = Catalog::from_paths(vec![directory.path().into()]).revision;
    assert_ne!(before, after);
    Ok(())
}

#[test]
fn preserves_empty_optional_fields_and_honors_precedence() -> anyhow::Result<()> {
    let high = tempfile::tempdir()?;
    let low = tempfile::tempdir()?;
    fs::write(
        high.path().join("hidden.desktop"),
        "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nHidden=true\n",
    )?;
    fs::write(
        low.path().join("hidden.desktop"),
        "[Desktop Entry]\nType=Application\nName=Visible lower copy\nExec=true\n",
    )?;
    fs::write(
        high.path().join("plain.desktop"),
        "[Desktop Entry]\nType=Application\nName=Plain\nExec=true\n",
    )?;

    let catalog = Catalog::from_paths(vec![high.path().into(), low.path().into()]);
    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(catalog.entries[0].id, "plain.desktop");
    assert_eq!(catalog.entries[0].icon, "");
    assert_eq!(catalog.entries[0].startup_class, "");
    Ok(())
}

use std::fs;

use super::Catalog;

#[test]
fn launch_metadata_changes_invalidate_catalog_even_when_presentation_is_identical()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("app.desktop");
    let original = "[Desktop Entry]\nType=Application\nName=App\nExec=old-command\nTerminal=false\nPath=/old\nDBusActivatable=false\nActions=inspect;\n[Desktop Action inspect]\nName=Inspect\nExec=old-action\n";
    fs::write(&path, original)?;
    let initial = Catalog::from_paths(vec![directory.path().into()]);
    for (from, to) in [
        ("Exec=old-command", "Exec=new-command"),
        ("Terminal=false", "Terminal=true"),
        ("Path=/old", "Path=/new"),
        ("DBusActivatable=false", "DBusActivatable=true"),
        ("Exec=old-action", "Exec=new-action"),
    ] {
        fs::write(&path, original.replace(from, to))?;
        let updated = Catalog::from_paths(vec![directory.path().into()]);
        assert_ne!(initial.revision, updated.revision, "{from}");
        assert_eq!(initial.entries[0].name, updated.entries[0].name);
        assert_eq!(initial.entries[0].actions, updated.entries[0].actions);
    }
    fs::write(&path, original)?;
    assert_eq!(
        initial.revision,
        Catalog::from_paths(vec![directory.path().into()]).revision
    );
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

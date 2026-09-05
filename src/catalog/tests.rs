use std::fs;

use super::Catalog;

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

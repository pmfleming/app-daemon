use std::io::{self, Read};

use serde_json::Value;
use shelllist_daemon_core::registry_names;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let mut source = String::new();
    io::stdin().read_to_string(&mut source)?;
    let value: Value = serde_json::from_str(&source)?;
    print!("{}", render(&value)?);
    Ok(())
}

fn render(value: &Value) -> Result<String> {
    let protocol = value
        .get("protocol")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("protocol registry is missing protocol"))?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::other("protocol registry is missing version"))?;
    let registry = value
        .get("registry")
        .or_else(|| value.pointer("/data/protocol"))
        .ok_or_else(|| io::Error::other("protocol registry is missing registry data"))?;
    let methods = registry_names(registry, "methods").map_err(io::Error::other)?;
    let streams = registry_names(registry, "streams").map_err(io::Error::other)?;

    let mut output = String::from(
        ".pragma library\n\n// Generated from the daemon-owned protocol registry. Do not edit.\n",
    );
    output.push_str(&format!(
        "var protocol = {};\n",
        serde_json::to_string(protocol)?
    ));
    output.push_str(&format!("var version = {version};\n"));
    output.push_str(&render_names("methods", &methods)?);
    output.push_str(&render_names("streams", &streams)?);
    Ok(output)
}

fn render_names(variable: &str, names: &[&str]) -> Result<String> {
    let mut output = format!("var {variable} = ({{\n");
    for name in names {
        let encoded = serde_json::to_string(name)?;
        output.push_str(&format!("    {encoded}: {encoded},\n"));
    }
    output.push_str("});\n");
    Ok(output)
}

use std::fmt::Write as _;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/vedavid.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/vedavid.proto"], &["proto"])?;
    compile_builtins()?;
    Ok(())
}

/// A broken built-in must break this build, before any pod runs it.
fn compile_builtins() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=builtins");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("builtins");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    paths.sort();

    let mut out = String::from("pub(crate) static BUILTINS: &[Builtin] = &[\n");
    for path in paths {
        let yaml = std::fs::read_to_string(&path)?;
        let tree = vedavid_dashboard_dsl::compile(&yaml).map_err(|diagnostics| {
            let joined: Vec<String> = diagnostics.iter().map(|d| d.to_string()).collect();
            format!(
                "the built-in dashboard {} does not compile:\n  {}",
                path.display(),
                joined.join("\n  ")
            )
        })?;
        writeln!(
            out,
            "    Builtin {{ id: {:?}, title: {:?}, hash: {:?}, schema: {}, json: {:?} }},",
            tree.id,
            tree.title,
            tree.hash,
            tree.schema,
            vedavid_dashboard_dsl::to_json(&tree)
        )?;
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR")?).join("builtins.rs");
    std::fs::write(dest, out)?;
    Ok(())
}

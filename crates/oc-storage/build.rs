use std::fmt::Write;

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/sql");
    let ids = std::fs::read_to_string(root.join("migration_ids.txt")).unwrap();
    let mut out = String::from("pub const MIGRATIONS: &[(&str, &str)] = &[\n");
    for id in ids.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let file = root.join("migration").join(format!("{id}.sql"));
        writeln!(out, "    ({id:?}, include_str!({:?})),", file.canonicalize().unwrap().display().to_string()).unwrap();
    }
    out.push_str("];\n");
    std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("migrations.rs"), out).unwrap();
    println!("cargo:rerun-if-changed=../../assets/sql");
}

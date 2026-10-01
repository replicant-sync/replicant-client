use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let output_file = target_dir().join("include").join("replicant.h");
    let config_file = PathBuf::from(&crate_dir).join("cbindgen.toml");

    std::fs::create_dir_all(output_file.parent().unwrap()).unwrap();

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(
            cbindgen::Config::from_file(&config_file).expect("Failed to read cbindgen.toml"),
        )
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(&output_file);

    println!(
        "cargo:rustc-env=REPLICANT_GENERATED_HEADER={}",
        output_file.display()
    );
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("Generated C header: {}", output_file.display());
}

fn target_dir() -> PathBuf {
    if let Ok(target) = env::var("CARGO_TARGET_DIR") {
        PathBuf::from(target)
    } else {
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("target")
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=COMET_DEFAULT_ENVIRONMENT");
    println!("cargo:rustc-check-cfg=cfg(comet_default_environment, values(\"canary\", \"staging\", \"production\"))");
    let environment = match std::env::var("COMET_DEFAULT_ENVIRONMENT") {
        Ok(value) if matches!(value.as_str(), "canary" | "staging" | "production") => value,
        Err(std::env::VarError::NotPresent) => "canary".into(),
        _ => panic!("COMET_DEFAULT_ENVIRONMENT must be canary, staging, or production"),
    };
    println!("cargo:rustc-cfg=comet_default_environment=\"{environment}\"");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/macos/Info-unbundled.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        // `cargo run` has no .app Info.plist. NSBundle also reads privacy
        // declarations from this Mach-O section, so source builds can request
        // microphone access without requiring a separate packaging step.
        println!(
            "cargo:rustc-link-arg-bin=zeron=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
    println!("cargo:rerun-if-changed=../../dist/windows/zeron.rc");
    println!("cargo:rerun-if-changed=../../dist/windows/zeron.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "../../dist/windows/zeron.rc",
            &["zeron"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("Windows app icon resource compilation failed");
    }
}

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/main.slint", config).expect("compile embedded Slint UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=SlimStore-icon.ico");
        println!("cargo:rerun-if-changed=assets/SlimStore.manifest");
        let version = concat!(env!("CARGO_PKG_VERSION"), ".0");
        winresource::WindowsResource::new()
            .set_icon("SlimStore-icon.ico")
            .set("ProductName", "SlimStore")
            .set("CompanyName", "NaeTech")
            .set("FileVersion", version)
            .set("ProductVersion", version)
            .set("FileDescription", "SlimStore — Discover. Install. Update.")
            .set("LegalCopyright", "SlimStore contributors. GPL-3.0-only.")
            .set("OriginalFilename", "SlimStore.exe")
            .set_manifest_file("assets/SlimStore.manifest")
            .compile()
            .expect("embed Windows resources");
    }
}

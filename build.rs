fn main() {
    // Embed Info.plist into the Mach-O so TCC finds usage strings and a stable
    // bundle identifier even when the binary runs outside an .app bundle.
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("support/Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}

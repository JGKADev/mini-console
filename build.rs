fn main() {
    // Embeds the app icon (resource 1) into the exe; the window uses it too.
    println!("cargo:rerun-if-changed=assets/app.ico");
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional().unwrap();
}

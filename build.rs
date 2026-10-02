fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/app.ico")
            .set("ProductName", "NKG Virtual Folder")
            .set("FileDescription", "NKG Virtual Folder")
            .set("InternalName", "nkg-virtual-folder")
            .set("OriginalFilename", "nkg-virtual-folder.exe")
            .compile()
            .expect("compile Windows application icon");
    }
}

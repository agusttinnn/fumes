//! Puts the logo on the Windows program, for Explorer, the taskbar and the
//! console window it opens in.

fn main() {
    println!("cargo:rerun-if-changed=assets/fumes.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/fumes.ico");
        resource.set("FileDescription", "fumes, a lightweight Steam client");
        resource.compile().expect("embedding the Windows icon");
    }
}

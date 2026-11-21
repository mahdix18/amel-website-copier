//! Windows builds embed the app icon (`assets/icon.ico`) as resource 1:
//! Explorer, the taskbar and shortcuts show it, and so does the window.
fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon_with_id("../../assets/icon.ico", "1")
            .set("ProductName", "Amel Website Copier")
            .set("FileDescription", "Amel Website Copier");
        resource.compile().expect("embedding the Windows icon");
    }
}

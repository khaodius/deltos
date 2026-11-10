//! Embeds the icon and version information in the Windows executable.

fn main() {
    println!("cargo::rerun-if-changed=../../assets/icons/deltos.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icons/deltos.ico");
        res.set("ProductName", "Deltos");
        res.set("FileDescription", "Deltos");
        res.set("CompanyName", "khaodius");
        res.set("LegalCopyright", "Licensed under the Apache License 2.0");
        if let Err(e) = res.compile() {
            println!("cargo::warning=could not embed the Windows icon: {e}");
        }
    }
}

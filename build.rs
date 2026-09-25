// Embed the app icon (and version metadata) into conduit.exe so Windows shows
// it in Search, Explorer, the taskbar and the Start menu. No-op off Windows.
fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=gui-winui/app.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("gui-winui/app.ico");
        // Best-effort: a missing rc toolchain shouldn't fail the whole build.
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed skipped: {e}");
        }
    }
}

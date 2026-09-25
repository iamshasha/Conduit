//! Best-effort GPU detection, used to recommend a local model size that will
//! actually fit. Windows reads the dedicated video memory of the largest
//! non-software DXGI adapter; other platforms report nothing (the UI then just
//! offers a small default).

/// The most capable GPU: (name, dedicated VRAM in bytes). None if detection
/// fails or there is no discrete adapter.
#[cfg(windows)]
pub fn best() -> Option<(String, u64)> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut best: Option<(String, u64)> = None;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let adapter: IDXGIAdapter1 = adapter;
            let desc = match adapter.GetDesc1() {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Skip the software/WARP rasterizer — it has no real VRAM.
            if DXGI_ADAPTER_FLAG(desc.Flags as i32) & DXGI_ADAPTER_FLAG_SOFTWARE != DXGI_ADAPTER_FLAG(0) {
                continue;
            }
            let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]).trim().to_string();
            let vram = desc.DedicatedVideoMemory as u64;
            if best.as_ref().map_or(true, |(_, v)| vram > *v) {
                best = Some((name, vram));
            }
        }
        best
    }
}

#[cfg(not(windows))]
pub fn best() -> Option<(String, u64)> {
    None
}

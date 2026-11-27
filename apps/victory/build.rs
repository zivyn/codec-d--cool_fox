//! victory.exe exports `D3D12SDKVersion` and `D3D12SDKPath` (main.rs): Windows'
//! Direct3D 12 reads them from the process's executable to load Microsoft's
//! newer runtime from `D3D12\` beside it. An executable exports nothing by
//! itself, so the linker is told to.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    }
}

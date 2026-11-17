fn main() {
    println!("cargo:rerun-if-changed=assets/windows.rc");
    println!("cargo:rerun-if-changed=assets/windows.manifest");
    println!("cargo:rerun-if-changed=assets/AppIcon.ico");

    // The distributed Windows build uses the MSVC toolchain and Windows SDK.
    // GNU targets remain usable for headless cross-platform code checks.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        embed_resource::compile(
            "assets/windows.rc",
            embed_resource::ParamsIncludeDirs(["assets"]),
        )
        .manifest_required()
        .expect("the Windows SDK resource compiler is required for Windows packaging");
    }
}

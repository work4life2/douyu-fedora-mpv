fn main() {
    // 用当前时间做构建指纹，便于确认运行的是哪次编译
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=DOUYU_BUILD={secs}");
    println!("cargo:rerun-if-changed=build.rs");
}

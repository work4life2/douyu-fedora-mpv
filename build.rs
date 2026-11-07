use std::time::UNIX_EPOCH;

/// 构建指纹 = src 下源码的最新修改时间，用来确认「跑的是哪一版代码」。
///
/// 坑：只声明 `rerun-if-changed=build.rs` 时，本脚本只在自身变化时重跑，
/// `DOUYU_BUILD` 会永远停在第一次编译的值上（改完代码也看不出来），
/// 所以这里必须额外盯住 `src` 目录。
fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");

    let mut newest = 0u64;
    if let Ok(rd) = std::fs::read_dir("src") {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                let secs = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                newest = newest.max(secs);
            }
        }
    }
    if newest == 0 {
        newest = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    }
    println!("cargo:rustc-env=DOUYU_BUILD={newest}");
}

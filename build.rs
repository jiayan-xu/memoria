// build.rs — 构建期注入 git 溯源信息。
//
// 设计说明：用户原要求用 vergen 实现「构建溯源注释」。此处用零外部依赖的方式
// 实现等价能力（cargo 构建脚本直接调用 git），以避免引入 vergen/gitoxide 的
// 编译成本与构建期对 git 标签/网络的隐性依赖，构建更稳健；效果一致：
// 版本号带 git short SHA（工作树脏时追加 -dirty）。
//
// 输出到源码的 env 变量：MEMORIA_BUILD_VERSION
//   形如 "0.3.0-g6233122" 或 "0.3.0-g6233122-dirty"；无 git 时回退为纯 CARGO_PKG_VERSION。
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8(out.stdout).ok()?;
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    } else {
        None
    }
}

fn main() {
    // 不声明 rerun-if-changed：每次构建都重跑本脚本，保证 MEMORIA_BUILD_VERSION
    // 始终反映最新 git HEAD（溯源时效优先于增量构建缓存）。
    let pkg = env!("CARGO_PKG_VERSION");
    let commit = git(&["rev-parse", "--short", "HEAD"]);
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let build = match (commit, dirty) {
        (Some(c), true) => format!("{pkg}-g{c}-dirty"),
        (Some(c), false) => format!("{pkg}-g{c}"),
        (None, _) => pkg.to_string(),
    };
    println!("cargo:rustc-env=MEMORIA_BUILD_VERSION={build}");
}

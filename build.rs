//! Inject build metadata (`git describe` + build date) as compile-time env
//! vars consumed by `cli::build_section` and the `--version` string.
//!
//! Both are optional: a build without git, or from a source tarball, simply
//! omits them and the binary falls back to the crate version / "unknown".

use std::process::Command;

fn main() {
    // Version: the tag if HEAD is on one (`v0.6.0`), otherwise
    // `<tag>-<n>-g<hash>`, otherwise a short hash; `-dirty` if the tree has
    // uncommitted changes. Released source tarballs without `.git` skip this.
    if let Some(describe) = git_describe() {
        println!("cargo:rustc-env=TSM_GIT_DESCRIBE={describe}");
        println!(
            "cargo:rustc-env=TSM_VERSION_LONG={describe} (built {date})",
            date = build_date()
        );
    } else {
        println!(
            "cargo:rustc-env=TSM_VERSION_LONG=v{ver} (built {date})",
            ver = env!("CARGO_PKG_VERSION"),
            date = build_date()
        );
    }
    println!("cargo:rustc-env=TSM_BUILD_DATE={}", build_date());

    // Re-run when HEAD moves (new commit/checkout) or the reproducible-build
    // timestamp changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Build date as `YYYY-MM-DD` (UTC). Honors `SOURCE_DATE_EPOCH` for
/// reproducible builds; otherwise uses the current wall-clock time.
fn build_date() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    civil_from_epoch(epoch)
}

/// Convert Unix seconds to a `YYYY-MM-DD` UTC date using Howard Hinnant's
/// days-from-civil inverse — no external date dependency.
fn civil_from_epoch(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

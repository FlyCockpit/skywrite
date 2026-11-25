// The C++ compiler and flags cc-rs (and so cmake-rs) use for a target,
// shared by build.rs and tests/build_cxx.rs (a `#[path]` module in both).

/// `(compiler, arguments)` from the environment, like cc-rs: the first of
/// CXX_<target>, CXX_<target_with_underscores>, HOST_CXX or TARGET_CXX (HOST_
/// when building for the host itself), CXX (default `c++`). A value naming an
/// existing file is the compiler even if it contains spaces; otherwise it is
/// split into words, a leading compiler wrapper is dropped and the rest are
/// arguments. Then every set CXXFLAGS variant is appended in cc-rs order:
/// CXXFLAGS, HOST_/TARGET_CXXFLAGS, CXXFLAGS_<target_with_underscores>,
/// CXXFLAGS_<target>.
pub fn compiler_and_flags(
    target: &str,
    host: &str,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&str) -> bool,
) -> (String, Vec<String>) {
    let set = |name: &String| env(name).filter(|value| !value.trim().is_empty());
    let [cxx_target, cxx_underscored, cxx_kind, cxx] = names("CXX", target, host);
    let compiler = [cxx_target, cxx_underscored, cxx_kind, cxx]
        .iter()
        .find_map(set)
        .unwrap_or_else(|| "c++".to_owned());
    let (cxx, mut words) = if exists(compiler.trim()) {
        (compiler.trim().to_owned(), Vec::new())
    } else {
        let mut words: Vec<String> = compiler.split_whitespace().map(str::to_owned).collect();
        let wrappers = ["ccache", "sccache", "distcc", "cachepot", "buildcache"];
        if words.len() > 1
            && std::path::Path::new(&words[0])
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| wrappers.contains(&stem))
        {
            words.remove(0);
        }
        let cxx = words.remove(0);
        (cxx, words)
    };
    let [flags_target, flags_underscored, flags_kind, flags] = names("CXXFLAGS", target, host);
    for name in [flags, flags_kind, flags_underscored, flags_target] {
        if let Some(value) = set(&name) {
            words.extend(value.split_whitespace().map(str::to_owned));
        }
    }
    (cxx, words)
}

/// `[NAME_<target>, NAME_<target_with_underscores>, HOST_/TARGET_NAME, NAME]`.
fn names(name: &str, target: &str, host: &str) -> [String; 4] {
    let kind = if target == host { "HOST" } else { "TARGET" };
    [
        format!("{name}_{target}"),
        format!("{name}_{}", target.replace('-', "_")),
        format!("{kind}_{name}"),
        name.to_owned(),
    ]
}

/// Every variable `compiler_and_flags` reads, for rerun-if-env-changed.
pub fn variables(target: &str, host: &str) -> Vec<String> {
    names("CXX", target, host)
        .into_iter()
        .chain(names("CXXFLAGS", target, host))
        .collect()
}

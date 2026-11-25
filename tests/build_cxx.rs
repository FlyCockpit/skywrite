//! build.rs picks the C++ compiler and flags for libstdc++.a the way cc-rs
//! does, so a static musl build links the target's archive, not the host's.
#[path = "../build_cxx.rs"]
mod build_cxx;

const CROSS: (&str, &str) = ("aarch64-unknown-linux-musl", "x86_64-unknown-linux-gnu");
const NATIVE: (&str, &str) = ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl");

fn pick(triples: (&str, &str), vars: &[(&str, &str)], files: &[&str]) -> (String, Vec<String>) {
    build_cxx::compiler_and_flags(
        triples.0,
        triples.1,
        &|name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        },
        &|path| files.contains(&path),
    )
}

#[test]
fn target_specific_compiler_wins_and_wrappers_are_dropped() {
    assert_eq!(pick(CROSS, &[], &[]), ("c++".into(), vec![]));
    let vars = [
        ("CXX", "g++"),
        ("TARGET_CXX", "aarch64-linux-musl-g++"),
        ("HOST_CXX", "host-g++"),
    ];
    assert_eq!(pick(CROSS, &vars, &[]).0, "aarch64-linux-musl-g++");
    // Building for the host itself, cc-rs reads HOST_ instead of TARGET_.
    assert_eq!(pick(NATIVE, &vars, &[]).0, "host-g++");
    assert_eq!(
        pick(
            CROSS,
            &[
                ("CXX", "g++"),
                (
                    "CXX_aarch64_unknown_linux_musl",
                    "ccache aarch64-linux-musl-g++ -march=armv8-a"
                ),
            ],
            &[]
        ),
        (
            "aarch64-linux-musl-g++".into(),
            vec!["-march=armv8-a".into()]
        )
    );
    assert_eq!(pick(CROSS, &[("CXX", "  ")], &[]).0, "c++");
    // An existing compiler path is kept whole, spaces included.
    assert_eq!(
        pick(
            CROSS,
            &[("CXX", "/opt/musl toolchain/bin/g++")],
            &["/opt/musl toolchain/bin/g++"]
        ),
        ("/opt/musl toolchain/bin/g++".into(), vec![])
    );
}

#[test]
fn every_cxxflags_variant_is_kept_in_cc_rs_order() {
    let vars = [
        ("CXX_aarch64_unknown_linux_musl", "clang++"),
        (
            "CXXFLAGS",
            "--target=aarch64-linux-musl --sysroot=/opt/aarch64-musl",
        ),
        ("TARGET_CXXFLAGS", "-fPIC"),
        ("HOST_CXXFLAGS", "-host-only"),
        ("CXXFLAGS_aarch64_unknown_linux_musl", "-O2"),
        ("CXXFLAGS_aarch64-unknown-linux-musl", "-g0"),
    ];
    let (cxx, flags) = pick(CROSS, &vars, &[]);
    assert_eq!(cxx, "clang++");
    assert_eq!(
        flags,
        [
            "--target=aarch64-linux-musl",
            "--sysroot=/opt/aarch64-musl",
            "-fPIC",
            "-O2",
            "-g0"
        ]
    );
    let native = pick(
        NATIVE,
        &[
            ("CXXFLAGS", "-a"),
            ("HOST_CXXFLAGS", "-b"),
            ("TARGET_CXXFLAGS", "-c"),
        ],
        &[],
    );
    assert_eq!(native.1, ["-a", "-b"]);
    let variables = build_cxx::variables(CROSS.0, CROSS.1);
    assert!(variables.contains(&"CXXFLAGS_aarch64-unknown-linux-musl".to_owned()));
    assert!(variables.contains(&"TARGET_CXX".to_owned()));
    assert!(build_cxx::variables(NATIVE.0, NATIVE.1).contains(&"HOST_CXX".to_owned()));
}

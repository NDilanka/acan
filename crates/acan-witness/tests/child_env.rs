use acan_witness::child_env;

const POISON: &[&str] = &[
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
    "DOCKER_TLS_VERIFY",
    "HTTPS_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "NO_PROXY",
    "LD_PRELOAD",
    "DYLD_INSERT_LIBRARIES",
    "BASH_ENV",
    "ENV",
    "NODE_OPTIONS",
    "META_API_KEY",
    "ACAN_AGENT_TOKEN",
];

#[test]
fn poisoned_environment_cannot_reach_a_spawned_child() {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
    child.args(["--exact", "poisoned_parent", "--ignored"]);
    for name in POISON {
        // Loader variables must not affect the test harness itself.
        if !name.starts_with("LD_") && !name.starts_with("DYLD_") {
            child.env(name, "acan-poison-canary");
        }
    }
    assert!(child.status().expect("poisoned subprocess").success());
}

#[test]
#[ignore = "subprocess helper"]
fn poisoned_parent() {
    assert_eq!(
        std::env::var("DOCKER_HOST").as_deref(),
        Ok("acan-poison-canary")
    );
    let status = child_env::std_command(std::env::current_exe().expect("test binary"))
        .args(["--exact", "inspect_child", "--ignored"])
        .status()
        .expect("sanitized subprocess");
    assert!(status.success());
}

#[test]
#[ignore = "subprocess helper"]
fn inspect_child() {
    for name in POISON {
        assert!(std::env::var_os(name).is_none(), "{name} leaked");
    }
}

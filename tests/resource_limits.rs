use know_things::{
    contracts::{Mode, Search},
    process::Worker,
    store::Store,
};
use std::{fs, process::Command};

#[test]
fn owned_process_descriptor_exhaustion_preserves_evidence_and_recovers() {
    const CHILD: &str = "KNOW_THINGS_DESCRIPTOR_LIMIT_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        // Restrict only a fresh child running this one fixture. Other tests and
        // terminal sessions retain their original resource limits.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "owned_process_descriptor_exhaustion_preserves_evidence_and_recovers",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let data = temporary.path().join("data");
    let mut store = Store::open(&data, None).unwrap();
    store.register("fixture", &source).unwrap();
    let job = store
        .stage("fixture", "guide.md", "Redis descriptor recovery policy.")
        .unwrap()
        .unwrap();
    store.activate(job).unwrap();
    let query = || Search {
        query: "descriptor".into(),
        collections: vec!["fixture".into()],
        top_k: 5,
        evidence_token_budget: 3000,
        deadline_ms: 5000,
        mode: Mode::Lexical,
        strict_hybrid: false,
    };
    let evidence = store.search(query()).unwrap().evidence.remove(0);
    let generation = store.generation().unwrap();
    let mut original = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: this child owns its limits. Preserve the hard limit so restoring
    // its soft limit needs no privileges. No allocations or pressure workload.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
        0
    );
    let restricted = libc::rlimit {
        rlim_cur: 0,
        rlim_max: original.rlim_max,
    };
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &restricted) },
        0
    );
    // Capture results, then restore before assertions/reporting can open files.
    let ordinary = Worker::spawn(&mut Command::new("/usr/bin/true"));
    let controlled = Worker::spawn_with_control(Command::new("/usr/bin/true"));
    let search = store.search(query());
    let context = store.context(&evidence.reference);
    let unchanged = store.generation();
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
        0
    );
    for result in [ordinary.map(|_| ()), controlled.map(|_| ())] {
        assert_eq!(result.unwrap_err().code, "resource_exhausted");
    }
    assert_eq!(search.unwrap().evidence[0].reference, evidence.reference);
    assert_eq!(context.unwrap(), evidence.text);
    assert_eq!(unchanged.unwrap(), generation);
    store.integrity().unwrap();
    // Capacity recovery permits new workers without changing canonical state.
    drop(Worker::spawn(&mut Command::new("/usr/bin/true")).unwrap());
    drop(Worker::spawn_with_control(Command::new("/usr/bin/true")).unwrap());
    drop(store);
    let reopened = Store::open(&data, None).unwrap();
    assert_eq!(reopened.generation().unwrap(), generation);
    assert_eq!(
        reopened.context(&evidence.reference).unwrap(),
        evidence.text
    );
    reopened.integrity().unwrap();
}

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn pager_pty_round_trip() {
    if std::env::var_os("MDRS_PAGER_PTY_CHILD").is_some() {
        mdrs::run_pager(&mdrs::PagerConfig {
            initial_source: b"# Pager smoke test\n\nPress q to exit.\n".to_vec(),
            label: "smoke.md".into(),
            ..Default::default()
        })
        .unwrap();
        return;
    }

    if Command::new("script").arg("--version").output().is_err() {
        return;
    }
    let test_binary = std::env::current_exe().unwrap();
    let command = format!(
        "{} --exact pager_pty_round_trip --nocapture",
        test_binary.display()
    );
    let mut child = Command::new("script")
        .args(["-qfec", &command, "/dev/null"])
        .env("MDRS_PAGER_PTY_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"q").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let terminal = String::from_utf8_lossy(&output.stdout);
    assert!(terminal.contains("\x1b[?1049h"));
    assert!(terminal.contains("\x1b[?1049l"));
}

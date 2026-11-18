#!/usr/bin/env python3
"""Exercise a real nextest run-wrapper hook against an unavailable daemon."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

wrapper = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="nextest-logmon-") as scratch:
    root = Path(scratch)
    (root / "src").mkdir()
    (root / ".config").mkdir()
    (root / "Cargo.toml").write_text('[package]\nname="wrapper-fixture"\nversion="0.1.0"\nedition="2021"\n')
    (root / "src/lib.rs").write_text(r'''
#[test] fn passing() {
    let id=std::env::var("LOGMON_TEST_TRACE_ID").unwrap();
    assert_eq!(id.len(),32);
    assert!(id.bytes().all(|b|b.is_ascii_hexdigit()));
}
#[test] fn retained_descriptor() {
    std::process::Command::new("/bin/sh").args(["-c","sleep 1"]).spawn().unwrap();
}
#[test] fn long_sleep() {
    std::thread::sleep(std::time::Duration::from_secs(10));
}
#[test] fn failing() {
    use std::io::Write;
    std::io::stdout().write_all(b"forwarded-stdout\n").unwrap();
    std::io::stderr().write_all(&vec![b'x';12000]).unwrap();
    panic!("verbatim failure marker");
}''')
    (root / ".config/nextest.toml").write_text(
        'experimental=["wrapper-scripts"]\n[scripts.wrapper.logmon]\ncommand='
        + json.dumps([str(wrapper)]) +
        '\n[[profile.default.scripts]]\nfilter="all()"\nrun-wrapper="logmon"\n')
    estimates=root/"durations.json"
    estimates.write_text(json.dumps({"rust:wrapper-fixture::passing":235}))
    with (root/".config/nextest.toml").open("a") as config:
        config.write('\n[profile.default]\nleak-timeout={period="50ms",result="fail"}\n'
                     '[[profile.default.overrides]]\nfilter="test(long_sleep)"\n'
                     'slow-timeout={period="300ms",terminate-after=1,grace-period="1s"}\n')
    env = dict(os.environ, STORE_TEST_LOG_ESTIMATES_PATH=str(estimates),STORE_TEST_LOG_TIMEOUTS_PATH=str(estimates), LOGMON_BROKER_SOCKET=str(root / "absent.sock"),
               LOGMON_CONFIG_DIR=str(root / "absent-config"),
               STORE_TEST_LOG_RUN_ID="fixture",STORE_TEST_LOG_EVENTS_PATH=str(root / "journal"))
    result = subprocess.run(["cargo","nextest","run","--offline","--retries","1","--no-fail-fast","-E","test(passing) | test(failing)"],
                            cwd=root,env=env,capture_output=True,timeout=60)
    assert result.returncode == 100, (result.returncode, result.stderr[-1200:])
    assert b"forwarded-stdout" in result.stderr and b"verbatim failure marker" in result.stderr
    rows = [json.loads(line) for line in (root/"journal").read_text().splitlines()]
    starts = [r["start"] for r in rows if "start" in r]
    ends = [r["event"] for r in rows if "event" in r]
    assert len(starts)==len(ends)==3 and len({r["execution_id"] for r in ends})==3
    passing=next(r for r in starts if r["name"].endswith("passing"))
    assert passing["estimated_ms"]==passing["test_timeout_ms"]==235
    assert {r["name"] for r in ends}=={"rust:wrapper-fixture::passing","rust:wrapper-fixture::failing"}
    for e in ends:
        assert e["selector"]=={"field":"test_attempt_id","value":e["execution_id"]}
        if e["name"].endswith("failing"):
            assert e["verdict"]=="FAIL" and len(e["fields"]["stderr"].encode())<=8192
            assert e["fields"]["stderr_truncated"] and "verbatim failure marker" in e["fields"]["stderr"]
    assert any(r["transport"]=="breaker_open" for r in rows)
    # A directly signalled child must be signalled from nextest's perspective too.
    proc=subprocess.run([wrapper,"/bin/sh","-c","kill -TERM $$"],env=dict(env,
        NEXTEST_BINARY_ID="fixture",NEXTEST_TEST_NAME="abort"),timeout=10)
    assert proc.returncode==-signal.SIGTERM, proc.returncode
    event=json.loads((root/"journal").read_text().splitlines()[-1])["event"]
    assert event["verdict"]=="ABORTED" and event["fields"]["signal"]==signal.SIGTERM
    for name, expected in [("retained_descriptor",b"LEAK"),("long_sleep",b"TIMEOUT")]:
        result=subprocess.run(["cargo","nextest","run","--offline","-E",f"test({name})"],cwd=root,
                              env=env,capture_output=True,timeout=12)
        assert result.returncode==100 and expected in result.stderr, (result.returncode,result.stderr[-900:])
    print("nextest: timeout, leaked descriptor, pass, failed retries, fresh identities, output bounds, breaker and signal verified")

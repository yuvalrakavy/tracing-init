#!/usr/bin/env python3
"""Matched warm nextest batches on cheap Rust tests and a private broker."""
import argparse
from importlib.util import module_from_spec, spec_from_file_location
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--logmon-repo", required=True, type=Path)
    p.add_argument("--wrapper", required=True, type=Path)
    p.add_argument("--tests", type=int, default=48)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--jobs", type=int, default=16)
    args = p.parse_args()
    wrapper, logmon = args.wrapper.resolve(), args.logmon_repo.resolve()
    rust_project = Path(__file__).resolve().parents[2]
    spec = spec_from_file_location("fixture",logmon/"scripts/test-nightly-capture.py")
    fixture = module_from_spec(spec)
    spec.loader.exec_module(fixture)
    bins = logmon/"target.noindex/debug"
    cases=["unwrapped","disabled","active","attributed"]
    results={case:[] for case in cases}
    calls={case:[] for case in cases}
    with tempfile.TemporaryDirectory(prefix="nxp-") as tmp:
        root=Path(tmp)
        (root/"src").mkdir()
        (root/".config").mkdir()
        (root/"Cargo.toml").write_text('[package]\nname="probe"\nversion="0.1.0"\nedition="2021"\n'
            '[dependencies]\ntracing="0.1"\ntracing-init={path='+json.dumps(str(rust_project))+',features=["logmon-test"]}\n')
        # Match the same dependency set and lock versions used for the tracing-init pilot.
        (root/"Cargo.lock").write_bytes((rust_project/"Cargo.lock").read_bytes())
        (root/".config/nextest.toml").write_text('experimental=["wrapper-scripts"]\n'
            '[scripts.wrapper.logmon]\ncommand='+json.dumps([str(wrapper)])+
            '\n[[profile.wrapped.scripts]]\nfilter="all()"\nrun-wrapper="logmon"\n')
        body='let mut x=0u64; for n in 0..256 { x=x.wrapping_add(std::hint::black_box(n)); } assert_eq!(x,32640); tracing::info!("complete");'
        (root/"src/lib.rs").write_text('\n'.join(f'#[{attribute}] fn {prefix}_{i}() {{ {body} }}'
            for prefix,attribute in [("plain","test"),("attr","tracing_init::logmon_test")]
            for i in range(args.tests)))
        with fixture.broker(bins/"logmon-broker", root/"broker") as (broker_env,gelf,http,_):
            env=dict(broker_env, CARGO_TARGET_DIR=str(rust_project/"target"),
                     LOGMON_TEST_GELF_ADDR=f"127.0.0.1:{gelf}",LOGMON_TEST_OTLP_ENDPOINT=f"http://127.0.0.1:{http}")
            for key in list(env):
                if key.startswith("STORE_TEST_LOG_") or key in ("STORE_DEV_TRACK","LOGMON_TEST_TRACE_ID"):
                    del env[key]
            subprocess.run(["cargo","test","--no-run","--offline","--quiet"],env=env,cwd=root,check=True)
            for n in range(args.rounds+1):
                order=cases[n%4:]+cases[:n%4]
                for arm in order:
                    record=arm+str(n)
                    journal=root/(record+".jsonl")
                    control=root/(record+"-control.jsonl")
                    active=arm in ("active","attributed")
                    child_env=dict(env)
                    if active:
                        child_env.update(STORE_TEST_LOG_RUN_ID=record,STORE_TEST_LOG_EVENTS_PATH=str(journal))
                        fixture.cli(bins/"logmon-mcp",env,"test-log","begin",run_id=record,jsonl_path=control,grace_period_ms=0)
                    profile="default" if arm=="unwrapped" else "wrapped"
                    prefix="attr_" if arm=="attributed" else "plain_"
                    cmd=["cargo","nextest","run","--offline","--profile",profile,"-j",str(args.jobs),"-E",f"test({prefix})"]
                    t=time.perf_counter()
                    proc=subprocess.run(cmd,cwd=root,env=child_env,text=True,capture_output=True,timeout=120)
                    wall=time.perf_counter()-t
                    assert proc.returncode==0, proc.stderr[-1000:]
                    summary=re.search(r"Summary\s+\[\s*([\d.]+)s\]\s+(\d+) tests run",proc.stderr)
                    assert summary and int(summary[2])==args.tests,proc.stderr[-500:]
                    if n:results[arm].append({"nextest_seconds":float(summary[1]),"wall_seconds":round(wall,3)})
                    if active:
                        close=fixture.cli(bins/"logmon-mcp",env,"test-log","close",run_id=record,wait_timeout_ms=5000)
                        assert close["finalization"]=="settled",close
                        if n:calls[arm].extend(map(json.loads,journal.read_text().splitlines()))
    def percentile(values,q):
        return sorted(values)[max(0,math.ceil(len(values)*q)-1)]
    data={"tests_per_batch":args.tests,"rounds":args.rounds,"jobs":args.jobs}
    for arm in cases:
        rows=results[arm]
        data[arm]={"batches":rows, "median_nextest_seconds":statistics.median(r["nextest_seconds"] for r in rows),
                   "median_wall_seconds":statistics.median(r["wall_seconds"] for r in rows)}
        for key,kind in [("start","start"),("event","end")]:
            attempted=[r for r in calls[arm] if key in r]
            acks=[r["latency_us"] for r in attempted if r["transport"]=="acknowledged"]
            if attempted:data[arm][kind]={"count":len(attempted),"acks":len(acks),
                "timeouts":sum(r["transport"]=="timeout" for r in attempted),
                "breaker_skips":sum(r["transport"]=="breaker_open" for r in attempted),
                "p50_ack_us":percentile(acks,.5) if acks else None,"p99_ack_us":percentile(acks,.99) if acks else None}
    print(json.dumps(data,indent=2))

if __name__=="__main__":
    main()

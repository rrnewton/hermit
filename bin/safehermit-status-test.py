#!/usr/bin/env python3
"""Check real exit and signal forwarding through a safehermit user service.

No Hermit binary or guest is used. The optional second argument is a cold
directory in which every command, stream, report and result is retained.
"""
import datetime
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def run(wrapper, retained, selected=None):
    rows = []
    cases = [
        ('exit-one', 1, 1, 1, 1, 'exit-code'),
        ('exit-134', 134, 134, 1, 134, 'exit-code'),
        ('sigabrt', 134, 1, 3, 6, 'core-dump'),
        ('sigkill', 137, 255, 2, 9, 'signal'),
    ]
    for name, wanted, runner, code, status, unit_result in cases:
        if selected is not None and selected != name:
            continue
        folder = retained / name
        folder.mkdir()
        action = {'exit-one': 'sys.exit(1)', 'exit-134': 'sys.exit(134)',
                  'sigabrt': 'os.kill(os.getpid(),signal.SIGABRT)',
                  'sigkill': 'os.kill(os.getpid(),signal.SIGKILL)'}[name]
        payload = (
            "import os,resource,signal,sys; "
            "resource.setrlimit(resource.RLIMIT_CORE,(0,0)); "
            "assert resource.getrlimit(resource.RLIMIT_CORE)==(0,0); "
            "print('native status control '+sys.argv[1],flush=True); "
            + action
        )
        # The control's actual core limit is set inside the service, since a
        # user manager does not inherit the test process's resource limits.
        argv = [str(wrapper), '--sh-deadline', '10s', '--sh-report',
                str(folder / 'report'), sys.executable, '-c', payload, name]
        environment = dict(os.environ)
        environment['SAFEHERMIT_LOG_ROOT'] = str(folder / 'logs')
        record = {'argv': argv, 'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'expected_caller_status': wanted, 'expected_runner_status': runner,
                  'expected_exec_main_code': code, 'expected_exec_main_status': status,
                  'expected_unit_result': unit_result}
        (folder / 'command.json').write_text(json.dumps(record, indent=2) + '\n')
        start = time.monotonic()
        try:
            with (folder/'stdout').open('wb') as out, (folder/'stderr').open('wb') as err:
                child = subprocess.run(argv, stdout=out, stderr=err, env=environment, timeout=30)
        except subprocess.TimeoutExpired:
            record.update(status=None, timeout=True, seconds=time.monotonic()-start)
            (folder/'result.json').write_text(json.dumps(record,indent=2)+'\n')
            raise
        record.update(status=child.returncode, seconds=time.monotonic()-start)
        (folder / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
        report = (folder / 'report').read_text()
        fields = {}
        for line in report.splitlines():
            if line.startswith('safehermit: ') and '=' in line:
                key, value = line[len('safehermit: '):].split('=', 1)
                assert key not in fields, ('duplicate field', key, report)
                fields[key] = value
        assert child.returncode == wanted, record
        stdout = (folder/'stdout').read_bytes()
        assert stdout == ('native status control '+name+'\n').encode(), stdout
        assert fields['bound.cgroup'].startswith('APPLIED:'), report
        assert fields['truncated'] == 'false', report
        assert fields['unit_result'] == unit_result, report
        assert fields['runner_exit_code'] == str(runner), report
        assert fields['exit_code'] == str(wanted), report
        assert fields['exec_main_code'] == str(code), report
        assert fields['exec_main_status'] == str(status), report
        assert int(fields['exec_main_pid']) > 0, report
        if code in [2, 3]:
            assert fields['process_signal'] == str(status), report
        else:
            assert 'process_signal' not in fields, report
        rows.append(record)
    assert len(rows) == (1 if selected else 4)
    (retained / 'result.json').write_text(json.dumps({'success': True, 'cases': rows}, indent=2)+'\n')
    print('PASS: '+', '.join(row['argv'][-1] for row in rows))


def main():
    wrapper = Path(sys.argv[1]).resolve() if len(sys.argv)>1 else Path(__file__).with_name('safehermit')
    selected = sys.argv[3] if len(sys.argv)>3 else None
    assert selected in [None, 'exit-one', 'exit-134', 'sigabrt', 'sigkill']
    if len(sys.argv)>2:
        retained = Path(sys.argv[2]).resolve()
        retained.mkdir(parents=True, exist_ok=False)
        run(wrapper, retained, selected)
    else:
        with tempfile.TemporaryDirectory(prefix='safehermit-status-test-') as directory:
            run(wrapper, Path(directory), selected)


if __name__ == '__main__':
    main()

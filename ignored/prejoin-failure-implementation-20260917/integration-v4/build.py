#!/usr/bin/python3
"""Bound local integration compile, actual inventories, and exact native controls."""
from pathlib import Path
import hashlib
import json
import os
import re
import runpy
import stat
import subprocess

HERE = Path(__file__).resolve().parent
PLAN_SHA256 = '50052bf566a59fa202d116659c5e9d5a97cf36e2e98ed65532fcd85999323911'


def main():
    raw = (HERE / 'build-plan.json').read_bytes()
    assert hashlib.sha256(raw).hexdigest() == PLAN_SHA256
    plan = json.loads(raw)
    helper = Path(plan['helpers']['path'])
    assert hashlib.sha256(helper.read_bytes()).hexdigest() == plan['helpers']['sha256']
    functions = runpy.run_path(str(helper), run_name='reviewed_helpers_only')
    require, digest, read_bounded = (functions[x] for x in ['require', 'digest', 'read_bounded'])
    write_new, check_executable = (functions[x] for x in ['write_new', 'check_executable'])

    def check_inputs():
        for repo in plan['repositories']:
            functions['check_inputs'](dict(repo, inputs=plan['inputs'],
                                          optional_cargo_configs=plan['optional_cargo_configs']))

    def select_artifacts(raw):
        artifacts, completed = {}, []
        for line in raw.splitlines():
            row = json.loads(line)
            if row.get('reason') == 'build-finished':
                completed.append(row.get('success'))
            if row.get('reason') != 'compiler-artifact' or row.get('executable') is None:
                continue
            require(row.get('profile', {}).get('test') is True, 'non-test executable in no-run build')
            expected = [entry for entry in plan['expected_artifacts']
                        if row.get('manifest_path') == entry['manifest']
                        and row.get('target', {}).get('name') == entry['name']
                        and row['target'].get('kind') == entry['kind']]
            require(len(expected) == 1, 'unexpected executable target: ' + str(row))
            entry = expected[0]
            require(entry['id'] not in artifacts, 'duplicate executable artifact')
            executable = Path(row['executable'])
            require(executable.is_absolute() and not executable.is_symlink(), 'nonregular executable path')
            require(executable.resolve(strict=True).is_relative_to(Path(plan['target_dir'])), 'executable escaped target')
            info = executable.stat()
            require(stat.S_ISREG(info.st_mode) and os.access(executable, os.X_OK), 'invalid executable type/mode')
            artifacts[entry['id']] = dict(path=str(executable), sha256=digest(executable),
                                         bytes=info.st_size, mode=info.st_mode & 0o7777,
                                         cargo_artifact=row, selection=entry['selection'])
        require(completed == [True], 'Cargo did not report one successful build')
        require(set(artifacts) == {row['id'] for row in plan['expected_artifacts']}, 'missing affected test artifact')
        return artifacts

    require(plan['execution'] == ['/usr/bin/python3', '-B', str(HERE / 'build.py')], 'wrong caller')
    require(plan['required_counts'] == {'detcore': 51, 'hermit': 9}, 'changed native population')
    require(len(set(plan['selected_tests']['detcore'])) == 51 and
            len(set(plan['selected_tests']['hermit'])) == 9, 'duplicate native selectors')
    root = Path(plan['run_root'])
    for path in [root, Path(plan['target_dir']), *[Path(row['out']) for row in plan['stages']]]:
        require(not path.exists() and not path.is_symlink(), 'retain every prior attempt: ' + str(path))
    check_inputs()
    root.mkdir(mode=0o700)
    Path(plan['tmpdir']).mkdir(mode=0o700)
    environment = {key: os.environ[key] for key in plan['environment_keys'] if key in os.environ}
    environment.update(plan['environment_fixed'])
    write_new(root / 'launch.json', dict(plan_sha256=PLAN_SHA256, caller_sha256=digest(__file__),
              execution=plan['execution'], cwd=str(Path.cwd()), environment=environment,
              repositories=plan['repositories'], scope=plan['scope']))
    records, artifacts, inventories = [], {}, {}
    active = None
    try:
        for step in plan['stages']:
            active = step['name']
            check_inputs()
            for artifact in artifacts.values():
                check_executable(artifact)
            argv = list(step['argv'])
            if step['artifact'] is not None:
                artifact = artifacts[step['artifact']]
                require(argv.count('<verified-compiled-test-executable>') == 1, 'unbound executable')
                argv[argv.index('<verified-compiled-test-executable>')] = artifact['path']
            else:
                require(active == 'compile' and not artifacts, 'unexpected compile stage')
            if active.startswith('native-'):
                require(set(inventories) == set(artifacts), 'native run before all inventories completed')
            write_new(root / (active + '-dispatch.json'), dict(argv=argv, cwd=step['cwd'],
                      artifact=artifacts.get(step['artifact']),
                      selected=plan['selected_tests'].get(step['artifact']) if active.startswith('native-') else None))
            with (root / (active + '-observer.stdout')).open('xb') as stdout, \
                 (root / (active + '-observer.stderr')).open('xb') as stderr:
                process = subprocess.run(argv, cwd=step['cwd'], env=environment,
                                         stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
            result_path = Path(step['out']) / 'result.json'
            result = json.loads(read_bounded(result_path, 1024**2))
            record = dict(stage=active, observer_exit=process.returncode, result_path=str(result_path),
                          result_sha256=digest(result_path), result=result)
            records.append(record)
            write_new(root / (active + '-readback.json'), record)
            functions['require_terminal'](result, process.returncode, step, root, environment)
            raw = read_bounded(Path(step['out']) / 'stdout', step['reader_limit_bytes'])
            read_bounded(Path(step['out']) / 'stderr', step['reader_limit_bytes'])
            if active == 'compile':
                artifacts = select_artifacts(raw)
                write_new(root / 'compiled-executables.json', artifacts)
            elif active.startswith('list-'):
                listed = [line[:-6] for line in raw.decode().splitlines() if line.endswith(': test')]
                require(len(listed) == len(set(listed)), 'duplicate listed identity')
                selected = plan['selected_tests'].get(step['artifact'], [])
                require(all(listed.count(name) == 1 for name in selected), 'selected test missing from intended library')
                inventories[step['artifact']] = dict(names=listed, count=len(listed), selected=selected,
                                                     raw_sha256=hashlib.sha256(raw).hexdigest())
                write_new(root / (active + '-inventory.json'), inventories[step['artifact']])
            else:
                expected = plan['required_counts'][step['artifact']]
                summaries = re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', raw.decode(), re.M)
                require(len(summaries) == 1 and summaries[0][:4] == (str(expected), '0', '0', '0'),
                        'wrong executed native population')
                write_new(root / (active + '-summary.json'), dict(selected=plan['selected_tests'][step['artifact']],
                          counts=summaries[0], raw_sha256=hashlib.sha256(raw).hexdigest()))
            check_inputs()
            for artifact in artifacts.values():
                check_executable(artifact)
    except Exception as error:
        write_new(root / 'summary.json', dict(status='failed', active_stage=active, error=str(error),
                  retained_stages=[row['stage'] for row in records], artifacts=artifacts,
                  inventories=inventories, scope='Retained failed attempt; no retry or guest/parity claim.'))
        raise
    write_new(root / 'summary.json', dict(status='passed', selected_count=60, artifacts=artifacts,
              inventories=inventories, observed=[dict(stage=row['stage'], exit=row['result']['wrapper_exit_code'],
                cpu_nsec=row['result']['final_accounting']['cpu_usage_nsec'],
                wall_seconds=row['result']['elapsed_seconds']) for row in records], scope=plan['scope']))
    print(json.dumps(dict(status='passed', summary=str(root / 'summary.json'))))


if __name__ == '__main__':
    main()

#!/usr/bin/python3
"""Compile hardware qualification targets and record actual inventories only."""
from pathlib import Path
import hashlib
import json
import os
import runpy
import stat
import subprocess

HERE = Path(__file__).resolve().parent
PLAN_SHA256 = '4ccb255ee9d7d8e56bfcaa67a1e81bf06d9f84b576a03daabce71a4efcf326f8'


def main():
    raw = (HERE / 'plan.json').read_bytes()
    assert hashlib.sha256(raw).hexdigest() == PLAN_SHA256
    plan = json.loads(raw)
    helper = Path(plan['helpers']['path'])
    assert hashlib.sha256(helper.read_bytes()).hexdigest() == plan['helpers']['sha256']
    functions = runpy.run_path(str(helper), run_name='reviewed_helpers_only')
    require, digest, read_bounded = (functions[name] for name in ['require', 'digest', 'read_bounded'])
    write_new, check_executable = (functions[name] for name in ['write_new', 'check_executable'])

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
            expected = [entry for entry in plan['expected_artifacts']
                        if row.get('manifest_path') == entry['manifest']
                        and row.get('target', {}).get('name') == entry['name']
                        and row['target'].get('kind') == entry['kind']
                        and row.get('profile', {}).get('test') is entry['test']]
            require(len(expected) == 1, 'unexpected executable target: ' + str(row))
            entry = expected[0]
            require(entry['id'] not in artifacts, 'duplicate executable artifact')
            require('kvm-execution-tests' in row['features'] and
                    'kvm-native-test-support' not in row['features'], 'wrong qualification features')
            executable = Path(row['executable'])
            require(executable.is_absolute() and not executable.is_symlink(), 'nonregular executable path')
            require(executable.resolve(strict=True).is_relative_to(Path(plan['target_dir'])), 'executable escaped target')
            info = executable.stat()
            require(stat.S_ISREG(info.st_mode) and os.access(executable, os.X_OK), 'invalid executable mode/type')
            artifacts[entry['id']] = dict(path=str(executable), sha256=digest(executable),
                                         bytes=info.st_size, mode=info.st_mode & 0o7777, cargo_artifact=row)
        require(completed == [True], 'Cargo did not report one successful build')
        required = {row['id'] for row in plan['expected_artifacts'] if row['required']}
        require(required <= set(artifacts), 'missing required qualification executable')
        require(artifacts['hermit-program']['path'] == str(Path(plan['target_dir']) / 'debug/hermit'),
                'normal Hermit executable path differs from CARGO_BIN_EXE_hermit')
        return artifacts

    require(plan['execution'] == ['/usr/bin/python3', '-B', str(HERE / 'build.py')], 'wrong caller')
    require([step['name'] for step in plan['stages']] ==
            ['compile', 'list-hermit', 'list-cli', 'list-kvm-harder'], 'unexpected execution stage')
    require(len(set(plan['original_cli_tests'])) == 24, 'changed original KVM CLI population')
    root = Path(plan['run_root'])
    target = Path(plan['target_dir'])
    require(plan['reuse_owned_target_cache'] is True and target.is_dir() and not target.is_symlink(),
            'expected owned Cargo cache is missing or not a directory')
    require(target.stat().st_uid == os.getuid() and target.resolve(strict=True).is_relative_to(
            Path(plan['repositories'][0]['source_root']) / 'target'), 'Cargo cache is outside the owned target')
    for path in [root, Path(plan['observer_root'])]:
        require(not path.exists() and not path.is_symlink(), 'retain prior attempt: ' + str(path))
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
                require(argv.count('<verified-compiled-test-executable>') == 1, 'unbound executable')
                argv[argv.index('<verified-compiled-test-executable>')] = artifacts[step['artifact']]['path']
            else:
                require(active == 'compile' and not artifacts, 'unexpected compile stage')
            write_new(root / (active + '-dispatch.json'), dict(argv=argv, cwd=step['cwd'],
                      artifact=artifacts.get(step['artifact'])))
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
            else:
                listed = [line[:-6] for line in raw.decode().splitlines() if line.endswith(': test')]
                require(len(listed) == len(set(listed)), 'duplicate listed identity')
                if step['artifact'] == 'hermit':
                    require(listed.count(plan['setup_test']) == 1, 'setup method is absent or duplicated')
                    require(not any(name.startswith('kvm_failure_tests::combined::') for name in listed),
                            'native support feature leaked into hardware selection')
                elif step['artifact'] == 'cli':
                    selected = sorted(name for name in listed if name.startswith('run_kvm_'))
                    require(selected == plan['original_cli_tests'], 'original KVM CLI identities changed')
                else:
                    require(listed.count(plan['pthread_test']) == 1, 'pthread control is absent or duplicated')
                inventories[step['artifact']] = dict(names=listed, count=len(listed),
                                                     raw_sha256=hashlib.sha256(raw).hexdigest())
                write_new(root / (active + '-inventory.json'), inventories[step['artifact']])
            check_inputs()
            for artifact in artifacts.values():
                check_executable(artifact)
        require(set(inventories) == set(plan['test_artifacts']), 'missing actual inventory')
    except Exception as error:
        write_new(root / 'summary.json', dict(status='failed', active_stage=active, error=str(error),
                  retained_stages=[row['stage'] for row in records], artifacts=artifacts,
                  inventories=inventories, scope=plan['scope']))
        raise
    write_new(root / 'summary.json', dict(status='passed', artifacts=artifacts, inventories=inventories,
              recurring_kvm_selection=plan['original_cli_tests'] + [plan['setup_test']],
              recurring_kvm_count=len(plan['original_cli_tests']) + 1,
              observed=[dict(stage=row['stage'], exit=row['result']['wrapper_exit_code'],
                cpu_nsec=row['result']['final_accounting']['cpu_usage_nsec'],
                wall_seconds=row['result']['elapsed_seconds']) for row in records], scope=plan['scope']))
    print(json.dumps(dict(status='passed', summary=str(root / 'summary.json'))))


if __name__ == '__main__':
    main()

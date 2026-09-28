#!/usr/bin/python3
"""One source-bound local integration lock refresh; no build or test."""
from pathlib import Path
import difflib
import hashlib
import json
import os
import runpy
import subprocess

HERE = Path(__file__).resolve().parent
PLAN_SHA256 = '1a8e42ecb2bd1a2c241b71b2455ff1a81a576f0a843923318337450b96c4ec6a'


def main():
    raw = (HERE / 'lock-plan.json').read_bytes()
    assert hashlib.sha256(raw).hexdigest() == PLAN_SHA256
    plan = json.loads(raw)
    helper = Path(plan['helpers']['path'])
    assert hashlib.sha256(helper.read_bytes()).hexdigest() == plan['helpers']['sha256']
    functions = runpy.run_path(str(helper), run_name='reviewed_helpers_only')
    require, digest, read_bounded = (functions[x] for x in ['require', 'digest', 'read_bounded'])
    write_new = functions['write_new']

    def check_inputs(after=False):
        inputs = [row for row in plan['inputs']
                  if not (after and row['path'] == plan['permitted_output_mutation'])]
        for repo in plan['repositories']:
            functions['check_inputs'](dict(repo, inputs=inputs,
                                          optional_cargo_configs=plan['optional_cargo_configs']))

    require(plan['execution'] == ['/usr/bin/python3', '-B', str(HERE / 'lock.py')], 'wrong caller')
    require(len(plan['stages']) == 1, 'unexpected stage population')
    stage = plan['stages'][0]
    require(stage['payload'][1:] == ['--config', str(HERE / 'local-patches.toml'),
                                    'update', '--workspace', '--offline'], 'changed operation')
    require(stage['cpu_usec'] == 5000000 and stage['wall_seconds'] == 15, 'changed bounds')
    for path in [plan['run_root'], plan['observer_root'], plan['target_dir']]:
        require(not Path(path).exists() and not Path(path).is_symlink(), 'retain earlier attempt: ' + path)
    check_inputs()
    root = Path(plan['run_root'])
    root.mkdir(mode=0o700)
    Path(plan['tmpdir']).mkdir(mode=0o700)
    environment = {key: os.environ[key] for key in plan['environment_keys'] if key in os.environ}
    environment.update(plan['environment_fixed'])
    write_new(root / 'launch.json', dict(plan_sha256=PLAN_SHA256, caller_sha256=digest(__file__),
              execution=plan['execution'], argv=stage['argv'], cwd=stage['cwd'], environment=environment,
              old_lock_sha256=digest(HERE / 'Cargo.lock.before'), repositories=plan['repositories']))
    record = dict(status='failed before completion')
    try:
        with (root / 'observer.stdout').open('xb') as stdout, (root / 'observer.stderr').open('xb') as stderr:
            process = subprocess.run(stage['argv'], cwd=stage['cwd'], env=environment,
                                     stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)
        result_path = Path(stage['out']) / 'result.json'
        result = json.loads(read_bounded(result_path, 1024**2))
        record.update(observer_exit=process.returncode, result_path=str(result_path),
                      result_sha256=digest(result_path), result=result)
        write_new(root / 'lock-readback.json', record)
        functions['require_terminal'](result, process.returncode, stage, root, environment)
        read_bounded(Path(stage['out']) / 'stdout', 1024**2)
        read_bounded(Path(stage['out']) / 'stderr', 1024**2)
        record['status'] = 'workspace lock refresh completed'
    except Exception as error:
        record['error'] = str(error)
        raise
    finally:
        before = (HERE / 'Cargo.lock.before').read_bytes()
        after = Path(plan['permitted_output_mutation']).read_bytes()
        with (HERE / 'Cargo.lock.after').open('xb') as output:
            output.write(after)
        patch = ''.join(difflib.unified_diff(before.decode().splitlines(keepends=True),
                       after.decode().splitlines(keepends=True),
                       fromfile='Cargo.lock.before', tofile='Cargo.lock.after'))
        with (HERE / 'Cargo.lock.patch').open('x') as output:
            output.write(patch)
        record.update(old_lock_sha256=hashlib.sha256(before).hexdigest(),
                      new_lock_sha256=hashlib.sha256(after).hexdigest(),
                      diff_sha256=digest(HERE / 'Cargo.lock.patch'),
                      scope='Local workspace lock refresh only; no compile, test inventory or test execution.')
        try:
            check_inputs(after=True)
            record['source_and_other_inputs_unchanged'] = True
        except Exception as error:
            record.update(source_and_other_inputs_unchanged=False, postcheck_error=str(error),
                          status='failed input identity check')
        write_new(root / 'summary.json', record)
    print(json.dumps(dict(status=record['status'], summary=str(root / 'summary.json'))))
    require(record['source_and_other_inputs_unchanged'], 'source/config changed during lock refresh')


if __name__ == '__main__':
    main()

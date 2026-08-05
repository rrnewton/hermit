'use strict';

const assert = require('node:assert/strict');
const {spawnSync} = require('node:child_process');
const {resolveTargets} = require('./resolve-reverie-pin-targets.cjs');

const EXPECTED_REPOSITORY = 'rrnewton/hermit';
const HEAD = '1111111111111111111111111111111111111111';
const QUEUE_HEAD = '2222222222222222222222222222222222222222';
const OTHER_HEAD = '3333333333333333333333333333333333333333';

function pullRequest(overrides = {}) {
  return {
    number: 1655,
    state: 'open',
    base: {ref: 'main', repo: {full_name: EXPECTED_REPOSITORY}},
    head: {
      sha: HEAD,
      ref: 'ci/merge-gate-no-gh-bootstrap',
      repo: {full_name: EXPECTED_REPOSITORY},
    },
    ...overrides,
  };
}

function context(eventName, payload, overrides = {}) {
  return {
    eventName,
    repo: {owner: 'rrnewton', repo: 'hermit'},
    payload: {repository: {full_name: EXPECTED_REPOSITORY}, ...payload},
    sha: HEAD,
    ref: 'refs/heads/ci/merge-gate-no-gh-bootstrap',
    ...overrides,
  };
}

function noApiClient() {
  return {
    rest: {
      pulls: {
        get: async () => {
          throw new Error('pull_request payload path unexpectedly called REST');
        },
      },
    },
    graphql: async () => {
      throw new Error('pull_request payload path unexpectedly called GraphQL');
    },
  };
}

async function expectRefusal(operation, pattern) {
  await assert.rejects(operation, pattern);
}

const tests = [];
function test(name, operation) {
  tests.push({name, operation});
}

test('pull_request payload resolves its exact head without an API lookup', async () => {
  const result = await resolveTargets({
    github: noApiClient(),
    context: context('pull_request', {number: 1655, pull_request: pullRequest()}),
    expectedRepository: EXPECTED_REPOSITORY,
  });
  assert.deepEqual(result, [{number: 1655, headSha: HEAD, baseRef: 'main'}]);
});

test('missing and mismatched pull-request identities are refused', async () => {
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context('pull_request', {number: 1655}),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /pull_request object is missing/,
  );
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context('pull_request', {number: 1654, pull_request: pullRequest()}),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /does not match expected/,
  );
});

test('wrong repository, base, and malformed head are refused', async () => {
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context(
          'pull_request',
          {number: 1655, pull_request: pullRequest()},
          {repo: {owner: 'attacker', repo: 'hermit'}},
        ),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /workflow repository attacker\/hermit is not/,
  );
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context('pull_request', {
          number: 1655,
          pull_request: pullRequest({
            base: {ref: 'release', repo: {full_name: EXPECTED_REPOSITORY}},
          }),
        }),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /base release is not main/,
  );
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context('pull_request', {
          number: 1655,
          pull_request: pullRequest({head: {sha: 'not-a-sha'}}),
        }),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /head must be a lowercase 40-hex/,
  );
});

test('workflow_dispatch REST lookup is exact repository, PR, ref, and head bound', async () => {
  let request;
  const github = {
    rest: {
      pulls: {
        get: async (input) => {
          request = input;
          return {data: pullRequest()};
        },
      },
    },
  };
  const result = await resolveTargets({
    github,
    context: context('workflow_dispatch', {inputs: {pr_number: '1655'}}),
    expectedRepository: EXPECTED_REPOSITORY,
  });
  assert.deepEqual(request, {owner: 'rrnewton', repo: 'hermit', pull_number: 1655});
  assert.deepEqual(result, [{number: 1655, headSha: HEAD, baseRef: 'main'}]);
});

test('workflow_dispatch refuses a stale head or dispatch from the wrong branch', async () => {
  const staleGithub = {
    rest: {
      pulls: {
        get: async () => ({
          data: pullRequest({head: {...pullRequest().head, sha: OTHER_HEAD}}),
        }),
      },
    },
  };
  await expectRefusal(
    () =>
      resolveTargets({
        github: staleGithub,
        context: context('workflow_dispatch', {inputs: {pr_number: '1655'}}),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /does not match dispatched head/,
  );

  const wrongRefGithub = {
    rest: {pulls: {get: async () => ({data: pullRequest()})}},
  };
  await expectRefusal(
    () =>
      resolveTargets({
        github: wrongRefGithub,
        context: context(
          'workflow_dispatch',
          {inputs: {pr_number: '1655'}},
          {ref: 'refs/heads/main'},
        ),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /head ref .* does not match main/,
  );
});

function mergeQueueNode(overrides = {}) {
  return {
    headCommit: {oid: QUEUE_HEAD},
    pullRequest: {
      number: 1655,
      state: 'OPEN',
      baseRefName: 'main',
      baseRepository: {nameWithOwner: EXPECTED_REPOSITORY},
      headRefOid: HEAD,
    },
    ...overrides,
  };
}

test('merge_group GraphQL lookup is exact queue head and base bound', async () => {
  let variables;
  const github = {
    graphql: async (query, input) => {
      assert.match(query, /headCommit \{ oid \}/);
      assert.match(query, /baseRepository \{ nameWithOwner \}/);
      variables = input;
      return {repository: {mergeQueue: {entries: {nodes: [mergeQueueNode()]}}}};
    },
  };
  const mergeContext = context(
    'merge_group',
    {
      merge_group: {
        head_sha: QUEUE_HEAD,
        head_ref: 'refs/heads/gh-readonly-queue/main/pr-1655',
        base_ref: 'refs/heads/main',
      },
    },
    {sha: QUEUE_HEAD, ref: 'refs/heads/gh-readonly-queue/main/pr-1655'},
  );
  const result = await resolveTargets({
    github,
    context: mergeContext,
    expectedRepository: EXPECTED_REPOSITORY,
  });
  assert.deepEqual(variables, {owner: 'rrnewton', name: 'hermit', branch: 'main'});
  assert.deepEqual(result, [{number: 1655, headSha: HEAD, baseRef: 'main'}]);
});

test('merge_group refuses wrong base, wrong workflow head, and unbound results', async () => {
  const github = {
    graphql: async () => ({
      repository: {mergeQueue: {entries: {nodes: [mergeQueueNode()]}}},
    }),
  };
  await expectRefusal(
    () =>
      resolveTargets({
        github,
        context: context(
          'merge_group',
          {merge_group: {head_sha: QUEUE_HEAD, head_ref: 'queue', base_ref: 'refs/heads/release'}},
          {sha: QUEUE_HEAD, ref: 'queue'},
        ),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /base .* is not refs\/heads\/main/,
  );
  await expectRefusal(
    () =>
      resolveTargets({
        github,
        context: context(
          'merge_group',
          {merge_group: {head_sha: QUEUE_HEAD, head_ref: 'queue', base_ref: 'refs/heads/main'}},
          {sha: OTHER_HEAD, ref: 'queue'},
        ),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /does not match merge-group head/,
  );

  const unboundGithub = {
    graphql: async () => ({
      repository: {
        mergeQueue: {entries: {nodes: [mergeQueueNode({headCommit: {oid: OTHER_HEAD}})]}},
      },
    }),
  };
  await expectRefusal(
    () =>
      resolveTargets({
        github: unboundGithub,
        context: context(
          'merge_group',
          {merge_group: {head_sha: QUEUE_HEAD, head_ref: 'queue', base_ref: 'refs/heads/main'}},
          {sha: QUEUE_HEAD, ref: 'queue'},
        ),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /no open .* merge-queue entry is bound/,
  );
});

test('unsupported non-PR events fail closed', async () => {
  await expectRefusal(
    () =>
      resolveTargets({
        github: noApiClient(),
        context: context('push', {}),
        expectedRepository: EXPECTED_REPOSITORY,
      }),
    /unsupported event push/,
  );
});

async function main() {
  if (process.env.EXPECT_GH_ABSENT === '1') {
    const probe = spawnSync('gh', ['--version'], {encoding: 'utf8'});
    assert.equal(probe.error?.code, 'ENOENT', 'test PATH unexpectedly contains gh');
  }

  let passed = 0;
  for (const {name, operation} of tests) {
    await operation();
    passed += 1;
    process.stdout.write(`ok ${passed} - ${name}\n`);
  }
  process.stdout.write(`PASS: ${passed}/${tests.length} trusted target-resolution brackets\n`);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
